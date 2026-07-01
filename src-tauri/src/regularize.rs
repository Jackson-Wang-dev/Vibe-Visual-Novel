/// AUTOSTAGE's free-form entry point: turns an author's free-form script (any writing style) into
/// the internal marker language (`#loc:`/`#hnt:`/`#cast:`/`#seq:`/`#branch:`/`#opt:`/`#node:`/
/// `#jump:`, see `world_state.rs`) via exactly one LLM call, then hands the result to
/// `world_state::derive_world_state_timeline` for deterministic parsing. The marker language is
/// purely an internal representation - no author is meant to read or write it directly. Structural
/// parse errors get fed back into the *same* call for self-correction (never a second agent/stage);
/// the parsed `WorldStateTimeline` is rendered into a human-readable confirmation view, since
/// "did the model understand this correctly" has no deterministic answer - only a human can judge
/// genuinely ambiguous free-form writing, so this view is the actual checkpoint before AUTOSTAGE's
/// existing skeleton/fill/validate pipeline runs.
use crate::{generation, world_state, BackendError};
use serde::Serialize;
use std::future::Future;
use std::path::Path;

/// Resource/character names known to the project, pulled from the same `list_known_*` helpers
/// `build_autostage_prompt` already uses - keeps the regularizer grounded in what actually exists
/// instead of letting it invent plausible-sounding names that don't resolve to real assets.
pub struct GroundingBundle {
    pub known_characters: Vec<String>,
    pub known_assets: Vec<String>,
    pub known_audio_channels: Vec<(String, Vec<String>)>,
    pub known_sound_effects: Vec<String>,
    pub known_shaders: Vec<String>,
}

impl GroundingBundle {
    pub fn collect(nova2_project_dir: &Path) -> Self {
        let audio_layout = generation::list_known_audio_layout(nova2_project_dir);
        let mut known_characters = generation::list_known_character_bind_names(nova2_project_dir);
        for display_name in generation::list_known_speaker_display_names(nova2_project_dir) {
            if !known_characters.iter().any(|n| n == &display_name) {
                known_characters.push(display_name);
            }
        }
        GroundingBundle {
            known_characters,
            known_assets: generation::list_known_asset_script_paths(nova2_project_dir),
            known_audio_channels: audio_layout.channel_tracks,
            known_sound_effects: audio_layout.one_shot_tracks,
            known_shaders: generation::list_known_shader_names(nova2_project_dir),
        }
    }
}

/// Carries the previous attempt's output plus the specific structural error it failed on, so the
/// retry call can make a targeted fix instead of starting over blind.
pub struct RegularizeRetry {
    pub previous_marker_text: String,
    pub error_description: String,
}

pub struct RegularizeRequest<'a> {
    pub free_script: &'a str,
    pub grounding: &'a GroundingBundle,
    /// `#node:` names already used by earlier chunks, when the input had to be split (see the
    /// module-level chunking note on `regularize_with_retry`) - passed as context to reduce
    /// accidental name collisions before `derive_world_state_timeline`'s own two-pass resolution
    /// would otherwise catch them.
    pub previously_used_node_names: &'a [String],
    pub retry: Option<&'a RegularizeRetry>,
}

fn format_known_list(label: &str, items: &[String]) -> Option<String> {
    if items.is_empty() {
        None
    } else {
        Some(format!("## {label}\n\n{}", items.join(", ")))
    }
}

/// Builds the regularize prompt: marker-language-only output contract, the full marker language
/// spec (there's no external reference doc for this - it's internal to VVN), the three rules most
/// likely to be gotten wrong (scene/location, entrance/exit, branch condition-vs-consequence),
/// grounding lists, and the free-form script itself.
pub fn build_regularize_prompt(request: &RegularizeRequest) -> String {
    let mut sections = vec![
        "你要把作者写的一份完全自由格式的剧本，理解并改写成 VVN 内部使用的“标记语言”文本。这是一个确定性解析器要读的中间表示，不是最终的 NovaScript，也不是给作者看的东西——只输出标记语言文本本身，不要输出解释、标题、Markdown 代码块或围栏。\n\
        \n\
        输出里只允许出现：对话/旁白行（`显示名::台词`，旁白可以是没有 `::` 的纯文本行）、`#loc:`、`#hnt:`、`#cast:`、`#branch:`/`#opt:`/`#node:`/`#jump:`（含 `cond=`/`mode=`）。不要自己编排 `#seq:` 演出序列的内容——演出驱动的段落（CG/场景高速轮换那种）是纯创作判断，应该由作者自己手写具体的演出调用，你不该替作者拍板节奏。唯一的例外：如果作者原文里有一段看起来已经是写好的引擎级代码（`show(...)`/`tint(...)` 这类调用混在文字里），原样把这一段包进 `#seq:` ... `#seq:` 之间透传出去，不要去“理解”或改写它。"
            .to_string(),
        "## 标记语言完整说明\n\n\
        - `#loc: <路径>`：标注从这一拍开始的背景，粘滞生效直到下一个 `#loc:` 改变它。不再决定节点划分，纯粹是给后续填充演出用的标注。\n\
        - `#hnt: <内容>`：标注这一拍及之后的具体技术要求（用什么音乐、动画等），粘滞生效直到下一个 `#hnt:`（留空表示清除）或下一个 `#loc:`。\n\
        - `#cast: +<名字>` / `#cast: -<名字>`：明确标注某角色从这一拍起在场/离场——这是作者写明的事实，必须捕获，不要丢给“从说话人推断在场角色”的兜底逻辑。\n\
        - `#seq:`（不带值）：切换“演出序列”透传区间的开/关，再写一次 `#seq:` 就是关闭。区间内的每一行原样透传，不会被当成对话/标记解析——只应该用来包裹作者自己已经写好的引擎代码，你自己不要往里面生成内容。\n\
        - `#branch:`：声明当前节点以分支结束，后面紧跟一个或多个 `#opt:` 行。\n\
        - `#opt: <目标节点名> | <按钮文案> | <mode> | <cond>`：用竖线分隔，只有目标节点名是必填的，其余字段留空表示不需要。`mode` 是 `normal`/`jump`/`show`/`enable` 之一，留空等同 `normal`。\n\
        - `#node: <名字>`：开始一个新的、显式命名的节点，只能通过某个 `#opt:` 的目标节点名或 `#jump:` 到达。\n\
        - `#jump: <目标节点名>`：让当前节点以跳转结束（而不是默认的“结束”），用于让多条分支汇合到同一个后续节点。"
            .to_string(),
        "## 三条最容易理解错的映射规则\n\n\
        1. 场景描写（“画面淡入到教室”“【教室】”）→ `#loc: <解析到的真实背景路径>`；如果描写里还带了转场方式（淡入、左切等），那是 `#hnt:` 技术要求，不要塞进 `#loc:` 本身。\n\
        2. 角色登场/离场的描写（“小王走了进来”“小王离开了”）→ `#cast: +小王` / `#cast: -小王`，作为明确事实记录下来，不要只指望靠台词推断。\n\
        3. 分支的“可见条件”和“选择后果”是两件不同的事，最容易混：“只有满足某条件这个选项才会出现”→ 写进该 `#opt:` 的 `cond=`（可见性门槛）；“选了这个选项之后，会让变量 m 变成 1”→ 这是选择的**后果**，不是条件，应该写成该选项目标节点（`#node: <dest>`）开头的一个设置变量的语句，`#opt:` 本身只负责指向那个目标节点，不要把后果写进 `cond=`。"
            .to_string(),
        "## 接地纪律\n\n\
        引用的角色名、背景/立绘路径、音轨、音效、shader 名必须来自下面提供的清单。如果作者写的角色确实不在清单里，按作者原文的名字照常写出对话行（不要编一个清单里没有的路径/资源名来凑），这类“新角色/新资源”会在你之后的人工确认步骤里被单独列出来，不需要你在这一步处理。"
            .to_string(),
    ];

    if let Some(list) = format_known_list("已知角色显示名/绑定名", &request.grounding.known_characters) {
        sections.push(list);
    }
    if let Some(list) = format_known_list("已知背景/立绘资源路径", &request.grounding.known_assets) {
        sections.push(list);
    }
    if !request.grounding.known_audio_channels.is_empty() {
        let lines: Vec<String> = request
            .grounding
            .known_audio_channels
            .iter()
            .map(|(channel, tracks)| format!("- {channel}: {}", tracks.join(", ")))
            .collect();
        sections.push(format!("## 已知音轨\n\n{}", lines.join("\n")));
    }
    if let Some(list) = format_known_list("已知音效", &request.grounding.known_sound_effects) {
        sections.push(list);
    }
    if let Some(list) = format_known_list("已知 VFX shader", &request.grounding.known_shaders) {
        sections.push(list);
    }
    if !request.previously_used_node_names.is_empty() {
        sections.push(format!(
            "## 之前已经用过的节点名（避免重名）\n\n{}",
            request.previously_used_node_names.join(", ")
        ));
    }

    if let Some(retry) = request.retry {
        sections.push(format!(
            "## 上一次尝试的问题\n\n上一次你输出的标记剧本解析时报了这个错误，请只针对这个问题做最小修正，不要推倒重写：{}\n\n上一次的完整输出：\n\n{}",
            retry.error_description, retry.previous_marker_text
        ));
    }

    sections.push(format!("## 作者的自由格式剧本原文\n\n{}", request.free_script));

    sections.join("\n\n")
}

/// Drives the deterministic backstop + retry loop described in the AUTOSTAGE plan: call the model,
/// try to parse the result, and on a structural `WorldStateError` feed the specific problem back
/// into the *same* call (never a second agent), capped at 3 total attempts. `call_model` is
/// injected rather than calling `llm::generate_script` directly so this loop is unit-testable
/// against a stub - the real caller passes a closure wrapping `generation::generate_with_prompt`.
///
/// Chunking note: this always sends the whole `free_script` in one call, matching the plan's
/// preferred/default path ("feed the whole script in one call; only split when it doesn't fit
/// context"). Splitting oversized scripts into author-paragraph-aligned chunks (never mid-branch)
/// is not yet implemented - large scripts may need to be split by the caller for now.
/// Returns the marker text that actually parsed (not just the parsed `WorldStateTimeline`) -
/// callers that need to hand this off to AUTOSTAGE's existing `dialogue_only_text` input (e.g. the
/// `autostage` tauri command) need the literal text, not a value reconstructed from the timeline,
/// since reassembling marker syntax from `WorldStateTimeline` would be lossy (it wouldn't round-
/// trip `#seq:` passthrough placement or branch declarations faithfully).
pub async fn regularize_with_retry<F, Fut>(
    free_script: &str,
    grounding: &GroundingBundle,
    base_label: &str,
    call_model: F,
) -> Result<(String, world_state::WorldStateTimeline), BackendError>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Result<String, BackendError>>,
{
    let mut retry: Option<RegularizeRetry> = None;

    for _attempt in 0..3 {
        let request = RegularizeRequest {
            free_script,
            grounding,
            previously_used_node_names: &[],
            retry: retry.as_ref(),
        };
        let prompt = build_regularize_prompt(&request);
        let marker_text = call_model(prompt).await?;

        match world_state::derive_world_state_timeline(&marker_text, base_label) {
            Ok(timeline) => return Ok((marker_text, timeline)),
            Err(error) => {
                retry = Some(RegularizeRetry { previous_marker_text: marker_text, error_description: error.describe() });
            }
        }
    }

    Err(BackendError::message(format!(
        "自由剧本规整未能在 3 次尝试内生成合法的标记剧本，最后一次错误：{}",
        retry.map(|r| r.error_description).unwrap_or_default()
    )))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmationBeatView {
    pub text: String,
    pub speaker: Option<String>,
    pub background: Option<String>,
    pub on_stage: Vec<String>,
    pub hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmationNodeView {
    pub name: Option<String>,
    pub beats: Vec<ConfirmationBeatView>,
    pub has_handwritten_content: bool,
    pub terminator_summary: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmationView {
    pub nodes: Vec<ConfirmationNodeView>,
    pub new_characters: Vec<String>,
    pub unresolved_resources: Vec<String>,
}

fn summarize_terminator(terminator: &world_state::NodeTerminator) -> String {
    match terminator {
        world_state::NodeTerminator::End => "结束".to_string(),
        world_state::NodeTerminator::Jump(dest) => format!("跳转到 \"{dest}\""),
        world_state::NodeTerminator::Branch(options) => {
            let parts: Vec<String> = options
                .iter()
                .map(|option| {
                    let mut parts = vec![format!("跳转到 \"{}\"", option.dest)];
                    if let Some(text) = &option.text {
                        parts.push(format!("文案「{text}」"));
                    }
                    if let Some(mode) = &option.mode {
                        parts.push(format!("mode={mode}"));
                    }
                    if let Some(cond) = &option.cond {
                        parts.push(format!("cond={cond}"));
                    }
                    parts.join("，")
                })
                .collect();
            format!("分支：{}", parts.join("；"))
        }
    }
}

/// Renders the parsed `WorldStateTimeline` into the human-readable view the author actually
/// reviews - raw marker text never reaches them. `new_characters`/`unresolved_resources` are
/// computed purely by diffing against the grounding bundle, deliberately not LLM-self-flagged (a
/// confirmed v1 scope decision - no `#unsure:`-style marker, to avoid surface the model has to
/// reliably know when to use). Note `new_characters` compares dialogue *display* names against the
/// grounding bundle's bind names, which aren't strictly the same namespace - this can over-flag a
/// genuinely-known character whose bind differs from their display name, but never under-flags
/// (silently treats something as known when it isn't), which is the safer failure direction here.
pub fn render_confirmation_view(timeline: &world_state::WorldStateTimeline, grounding: &GroundingBundle) -> ConfirmationView {
    let nodes = timeline
        .nodes
        .iter()
        .map(|node| {
            let beats: Vec<ConfirmationBeatView> = node
                .items
                .iter()
                .filter_map(|item| match item {
                    world_state::NodeItem::Moment(moment) => Some(ConfirmationBeatView {
                        text: moment.text.clone(),
                        speaker: moment.speaker.clone(),
                        background: moment.background.clone(),
                        on_stage: moment.on_stage.clone(),
                        hint: moment.active_hint.clone(),
                    }),
                    world_state::NodeItem::RawPassthrough(_) => None,
                })
                .collect();
            let has_handwritten_content = node.items.iter().any(|item| matches!(item, world_state::NodeItem::RawPassthrough(_)));
            ConfirmationNodeView {
                name: node.name.clone(),
                beats,
                has_handwritten_content,
                terminator_summary: summarize_terminator(&node.terminator),
            }
        })
        .collect();

    let new_characters: Vec<String> = timeline
        .distinct_speakers
        .iter()
        .filter(|speaker| !grounding.known_characters.iter().any(|known| known == *speaker))
        .cloned()
        .collect();

    let mut unresolved_resources: Vec<String> = Vec::new();
    for node in &timeline.nodes {
        for item in &node.items {
            if let world_state::NodeItem::Moment(moment) = item {
                if let Some(background) = &moment.background {
                    let known = grounding.known_assets.iter().any(|known| known == background);
                    if !known && !unresolved_resources.contains(background) {
                        unresolved_resources.push(background.clone());
                    }
                }
            }
        }
    }

    ConfirmationView { nodes, new_characters, unresolved_resources }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_grounding() -> GroundingBundle {
        GroundingBundle {
            known_characters: Vec::new(),
            known_assets: Vec::new(),
            known_audio_channels: Vec::new(),
            known_sound_effects: Vec::new(),
            known_shaders: Vec::new(),
        }
    }

    #[test]
    fn build_regularize_prompt_instructs_marker_language_only_output() {
        let grounding = empty_grounding();
        let request = RegularizeRequest { free_script: "随便写的剧本", grounding: &grounding, previously_used_node_names: &[], retry: None };
        let prompt = build_regularize_prompt(&request);
        assert!(prompt.contains("只输出标记语言文本本身"));
        assert!(prompt.contains("#branch:"));
        assert!(prompt.contains("#cast:"));
    }

    #[test]
    fn build_regularize_prompt_includes_cond_vs_consequence_distinction() {
        let grounding = empty_grounding();
        let request = RegularizeRequest { free_script: "剧本", grounding: &grounding, previously_used_node_names: &[], retry: None };
        let prompt = build_regularize_prompt(&request);
        assert!(prompt.contains("可见性门槛"));
        assert!(prompt.contains("后果"));
    }

    #[test]
    fn build_regularize_prompt_includes_seq_passthrough_exception_for_pre_existing_engine_code() {
        let grounding = empty_grounding();
        let request = RegularizeRequest { free_script: "剧本", grounding: &grounding, previously_used_node_names: &[], retry: None };
        let prompt = build_regularize_prompt(&request);
        assert!(prompt.contains("原样把这一段包进"));
    }

    #[test]
    fn build_regularize_prompt_includes_retry_error_when_present() {
        let grounding = empty_grounding();
        let retry = RegularizeRetry { previous_marker_text: "#opt: a | 选A".to_string(), error_description: "缺少 #branch:".to_string() };
        let request = RegularizeRequest { free_script: "剧本", grounding: &grounding, previously_used_node_names: &[], retry: Some(&retry) };
        let prompt = build_regularize_prompt(&request);
        assert!(prompt.contains("缺少 #branch:"));
        assert!(prompt.contains("#opt: a | 选A"));
    }

    #[test]
    fn render_confirmation_view_flags_unknown_character_as_new() {
        let grounding = empty_grounding();
        let timeline = world_state::derive_world_state_timeline("二宫::你好\n", "ch1_autostage").unwrap();
        let view = render_confirmation_view(&timeline, &grounding);
        assert_eq!(view.new_characters, vec!["二宫".to_string()]);
    }

    #[test]
    fn render_confirmation_view_omits_known_characters_and_resources_from_new_unresolved_lists() {
        let grounding = GroundingBundle {
            known_characters: vec!["二宫".to_string()],
            known_assets: vec!["backgrounds/room".to_string()],
            known_audio_channels: Vec::new(),
            known_sound_effects: Vec::new(),
            known_shaders: Vec::new(),
        };
        let timeline = world_state::derive_world_state_timeline("#loc: backgrounds/room\n二宫::你好\n", "ch1_autostage").unwrap();
        let view = render_confirmation_view(&timeline, &grounding);
        assert!(view.new_characters.is_empty());
        assert!(view.unresolved_resources.is_empty());
    }

    #[test]
    fn render_confirmation_view_flags_unresolved_loc_path() {
        let grounding = empty_grounding();
        let timeline = world_state::derive_world_state_timeline("#loc: backgrounds/unknown\n二宫::你好\n", "ch1_autostage").unwrap();
        let view = render_confirmation_view(&timeline, &grounding);
        assert_eq!(view.unresolved_resources, vec!["backgrounds/unknown".to_string()]);
    }

    #[test]
    fn render_confirmation_view_summarizes_branch_terminator_with_all_option_fields() {
        let grounding = empty_grounding();
        let text = "#branch:\n#opt: a | 选A | show | v_flag > 0\n#node: a\n旁白文字。\n";
        let timeline = world_state::derive_world_state_timeline(text, "ch1_autostage").unwrap();
        let view = render_confirmation_view(&timeline, &grounding);
        let summary = &view.nodes[0].terminator_summary;
        assert!(summary.contains("\"a\""));
        assert!(summary.contains("选A"));
        assert!(summary.contains("mode=show"));
        assert!(summary.contains("cond=v_flag > 0"));
    }

    #[tokio::test]
    async fn regularize_with_retry_feeds_structural_error_back_and_succeeds_on_second_attempt() {
        let grounding = empty_grounding();
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let call_count_clone = call_count.clone();
        let result = regularize_with_retry("剧本", &grounding, "ch1_autostage", move |prompt: String| {
            let call_count = call_count_clone.clone();
            async move {
                let attempt = call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if attempt == 0 {
                    Ok("#opt: a | 选A\n".to_string())
                } else {
                    assert!(prompt.contains("OptWithoutBranch") || prompt.contains("前面没有对应的"));
                    Ok("二宫::你好\n".to_string())
                }
            }
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn regularize_with_retry_gives_up_after_three_attempts() {
        let grounding = empty_grounding();
        let result = regularize_with_retry("剧本", &grounding, "ch1_autostage", |_prompt: String| async { Ok("#opt: a | 选A\n".to_string()) }).await;
        assert!(result.is_err());
    }
}
