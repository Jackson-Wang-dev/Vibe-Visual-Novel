use crate::{character_template, llm, BackendError, PreviewBridgeError};
use serde::Deserialize;
use std::{fs, path::Path};

const NEW_CHARS_PREFIX: &str = "#NEWCHARS:";
const ASSET_ROOTS: [&str; 2] = ["standings", "backgrounds"];
const PATH_ARG_CALLS: [&str; 6] = ["show", "trans_fade", "trans_left", "trans_right", "trans_up", "trans_down"];

#[derive(Debug, Deserialize)]
pub struct NewCharacterSpec {
    node: String,
    bind: String,
    folder: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AssetPathIssue {
    pub written_path: String,
    pub suggested_path: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct AtmospherePlan {
    #[serde(default)]
    pub sound: String,
    #[serde(default)]
    pub text_presentation: String,
    #[serde(default)]
    pub visual_color: String,
    #[serde(default)]
    pub visual_animation: String,
    #[serde(default)]
    pub visual_vfx: String,
}

impl AtmospherePlan {
    fn is_empty(&self) -> bool {
        [&self.sound, &self.text_presentation, &self.visual_color, &self.visual_animation, &self.visual_vfx]
            .into_iter()
            .all(|field| field.trim().is_empty())
    }
}

/// Scans scene/game.tscn for existing `_bindName = "..."` entries (the same node shape
/// character_template::register_character writes) so the generation prompt can hand the model a
/// concrete list of real character names instead of letting it invent plausible-looking fakes.
/// Best-effort: a missing/unreadable game.tscn just yields an empty list rather than failing the
/// whole generation request over a nice-to-have hint.
pub(crate) fn list_known_character_bind_names(project_dir: &Path) -> Vec<String> {
    let game_tscn_path = project_dir.join("scene").join("game.tscn");
    let Ok(content) = fs::read_to_string(&game_tscn_path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|line| line.trim().strip_prefix("_bindName = \""))
        .filter_map(|rest| rest.split('"').next())
        .map(str::to_string)
        .collect()
}

/// Walks `resources/standings` and `resources/backgrounds` (the same two roots
/// AppRuntime::list_asset_files indexes) and converts each image file into the path string
/// NovaScript actually expects in a `show`/`trans*` call - relative to `resources/`, no extension,
/// forward slashes (eg. `resources/backgrounds/room.png` -> `backgrounds/room`). Used both to
/// ground the generation prompt with real paths and, after generation, to catch the model
/// dropping a `backgrounds/`/`standings/` prefix it invented from training data instead of reading
/// the project's actual layout.
pub(crate) fn list_known_asset_script_paths(project_dir: &Path) -> Vec<String> {
    let mut paths = Vec::new();
    for root in ASSET_ROOTS {
        collect_script_paths(&project_dir.join("resources").join(root), root, &mut paths);
    }
    paths.sort();
    paths
}

fn collect_script_paths(dir: &Path, script_prefix: &str, output: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            let child_prefix = format!("{script_prefix}/{}", entry.file_name().to_string_lossy());
            collect_script_paths(&path, &child_prefix, output);
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if !is_image_extension(&path) {
            continue;
        }
        output.push(format!("{script_prefix}/{stem}"));
    }
}

fn is_image_extension(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()).map(|ext| ext.to_ascii_lowercase()).as_deref(),
        Some("png") | Some("jpg") | Some("jpeg") | Some("webp") | Some("gif")
    )
}

/// Finds every genuine call to `call_name(` in `script` and returns the byte offset right after
/// the opening paren for each one. A plain `str::find` would also match `call_name(` as a
/// trailing substring of a longer identifier (e.g. searching for `"play("` would also match inside
/// `"video_play("`), so this additionally checks the byte right before the match isn't an
/// identifier character.
fn find_call_starts<'a>(script: &'a str, call_name: &str) -> Vec<usize> {
    let call_prefix = format!("{call_name}(");
    let mut starts = Vec::new();
    let mut search_from = 0;
    while let Some(relative_index) = script[search_from..].find(&call_prefix) {
        let match_start = search_from + relative_index;
        let is_real_call_start = match script[..match_start].chars().next_back() {
            Some(previous_char) => !previous_char.is_alphanumeric() && previous_char != '_',
            None => true,
        };
        if is_real_call_start {
            starts.push(match_start + call_prefix.len());
        }
        search_from = match_start + call_prefix.len();
    }
    starts
}

/// Best-effort static check that runs before spending a Godot reload round trip: Godot's
/// `load()` on a missing texture path just returns null (see graphics.gd's `show()`), so a
/// hallucinated path like `"room"` instead of `"backgrounds/room"` never surfaces as a
/// PreviewBridge parser/runtime error - the reload silently "succeeds" with an invisible
/// background. Scans `show`/`trans*` calls' image-path argument as a plain quoted-string literal
/// (good enough for NovaScript's call syntax; doesn't attempt full expression parsing). Skips
/// `show(obj, ...)` calls whose `obj` is a known character bind name, since for composite-sprite
/// objects the 2nd argument is a pose name (alias table lookup), not a resource path.
pub(crate) fn find_unknown_asset_paths(script: &str, known_paths: &[String], known_characters: &[String]) -> Vec<AssetPathIssue> {
    let mut issues = Vec::new();
    for call_name in PATH_ARG_CALLS {
        for call_start in find_call_starts(script, call_name) {
            let Some((obj, image_arg)) = extract_call_args(&script[call_start..], call_name) else {
                continue;
            };
            if call_name == "show" && known_characters.iter().any(|name| name == obj) {
                continue;
            }
            if known_paths.iter().any(|known| known == image_arg) {
                continue;
            }
            let suggested_path = ASSET_ROOTS
                .iter()
                .map(|root| format!("{root}/{image_arg}"))
                .find(|candidate| known_paths.contains(candidate));
            issues.push(AssetPathIssue {
                written_path: image_arg.to_string(),
                suggested_path,
            });
        }
    }
    issues
}

/// `show(obj, image_path, ...)` / `trans*(obj, image_name_or_func, ...)` both put the relevant
/// values in the first two positional arguments. The 2nd slot can also be a `Callable` (camera
/// transitions running a `func(): ...` block instead of naming an image) - those aren't asset
/// paths, so they're skipped rather than misreported as unknown paths.
fn extract_call_args<'a>(rest: &'a str, call_name: &str) -> Option<(&'a str, &'a str)> {
    let (obj, after_obj) = extract_quoted_arg(rest)?;
    let comma_index = after_obj.find(',')?;
    let after_first_arg = after_obj[comma_index + 1..].trim_start();
    if call_name != "show" && after_first_arg.starts_with("func") {
        return None;
    }
    let (image_arg, _) = extract_quoted_arg(after_first_arg)?;
    Some((obj, image_arg))
}

fn extract_quoted_arg(rest: &str) -> Option<(&str, &str)> {
    let trimmed = rest.trim_start();
    let quote_char = trimmed.chars().next()?;
    if quote_char != '"' && quote_char != '\'' {
        return None;
    }
    let body = &trimmed[1..];
    let end_index = body.find(quote_char)?;
    Some((&body[..end_index], &body[end_index + 1..]))
}

/// Generic "first two positional args are both quoted strings" extractor, used by the audio
/// checks below (`play`/`fade_in`'s `(channel, track_name, ...)`) - unlike `extract_call_args`,
/// there's no `Callable`-skipping special case to apply here.
fn extract_two_quoted_args(rest: &str) -> Option<(&str, &str)> {
    let (first, after_first) = extract_quoted_arg(rest)?;
    let comma_index = after_first.find(',')?;
    let (second, _) = extract_quoted_arg(&after_first[comma_index + 1..])?;
    Some((first, second))
}

/// Per-channel known track names (`channel_tracks`, e.g. `("bgm", ["prelude", "qianye", ...])`)
/// plus the dedicated one-shot `sound()` pool (`one_shot_tracks`) - see `list_known_audio_layout`.
#[derive(Debug, Default, Clone)]
pub struct AudioLayout {
    pub channel_tracks: Vec<(String, Vec<String>)>,
    pub one_shot_tracks: Vec<String>,
}

/// Reads `scene/game.tscn`'s Audio channel nodes to ground both the generation prompt and the
/// post-generation validator in the project's *actual* track files, instead of letting the model
/// guess plausible-sounding names (the bug this fixes: `sound("heartbeat", 0.7)` - there is no
/// `resources/audio/sound/heartbeat.ogg`, only `clap`/`flap`/`rain`). Channels are
/// `AudioPlayerController`-bound nodes under the `Audio` node, each carrying a `_bindName` (the
/// channel name scripts pass to `play`/`fade_in`, e.g. `"bgm"`/`"bgs"`) and an `_audioFolder` -
/// which is which folder under `resources/audio/` actually backs that channel (notably `"bgs"`
/// resolves to the shared `sound` folder, not a `bgs` folder - see game.tscn). The lone
/// `SoundController`-bound node (no `_bindName`, just `_audioFolder`) backs the one-shot `sound()`
/// call instead of a channel.
pub(crate) fn list_known_audio_layout(project_dir: &Path) -> AudioLayout {
    let tscn_path = project_dir.join("scene").join("game.tscn");
    let Ok(content) = fs::read_to_string(&tscn_path) else {
        return AudioLayout::default();
    };

    let mut channel_folders = Vec::new();
    let mut one_shot_folder = None;
    let mut current_bind: Option<String> = None;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("[node ") {
            current_bind = None;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("_bindName = \"") {
            current_bind = rest.split('"').next().map(str::to_string);
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("_audioFolder = \"") else {
            continue;
        };
        let Some(folder) = rest.split('"').next() else {
            continue;
        };
        match current_bind.take() {
            Some(bind) => channel_folders.push((bind, folder.to_string())),
            None => one_shot_folder = Some(folder.to_string()),
        }
    }

    let channel_tracks = channel_folders
        .into_iter()
        .map(|(channel, folder)| (channel, list_audio_track_names(project_dir, &folder)))
        .collect();
    let one_shot_tracks = one_shot_folder.map(|folder| list_audio_track_names(project_dir, &folder)).unwrap_or_default();
    AudioLayout { channel_tracks, one_shot_tracks }
}

fn list_audio_track_names(project_dir: &Path, folder: &str) -> Vec<String> {
    let dir = project_dir.join("resources").join("audio").join(folder);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("ogg"))
        .filter_map(|entry| entry.path().file_stem().and_then(|stem| stem.to_str()).map(str::to_string))
        .collect();
    names.sort();
    names
}

const AUDIO_TRACK_CALLS: [&str; 2] = ["play", "fade_in"];

/// Mirrors `find_unknown_asset_paths` for `play(channel, track_name, ...)` /
/// `fade_in(channel, track_name, ...)`. Only checks tracks for channels that are actually known
/// (from `list_known_audio_layout`) - an unrecognized channel name is a different kind of mistake
/// this check isn't trying to catch.
pub(crate) fn find_unknown_audio_tracks(script: &str, layout: &AudioLayout) -> Vec<AssetPathIssue> {
    let mut issues = Vec::new();
    for call_name in AUDIO_TRACK_CALLS {
        for call_start in find_call_starts(script, call_name) {
            let Some((channel, track)) = extract_two_quoted_args(&script[call_start..]) else {
                continue;
            };
            let Some((_, known_tracks)) = layout.channel_tracks.iter().find(|(name, _)| name == channel) else {
                continue;
            };
            if known_tracks.iter().any(|known| known == track) {
                continue;
            }
            issues.push(AssetPathIssue {
                written_path: format!("{channel}: {track}"),
                suggested_path: None,
            });
        }
    }
    issues
}

/// Mirrors `find_unknown_asset_paths` for the one-shot `sound(track_name, vol)` call.
pub(crate) fn find_unknown_sound_tracks(script: &str, one_shot_tracks: &[String]) -> Vec<AssetPathIssue> {
    let mut issues = Vec::new();
    for call_start in find_call_starts(script, "sound") {
        let Some((track, _)) = extract_quoted_arg(&script[call_start..]) else {
            continue;
        };
        if one_shot_tracks.iter().any(|known| known == track) {
            continue;
        }
        issues.push(AssetPathIssue {
            written_path: track.to_string(),
            suggested_path: None,
        });
    }
    issues
}

/// Lists real `vfx()`-loadable shaders: `resources/shaders/*.gdshader`. Deliberately excludes
/// `.gdshaderinc` files (eg. `noise.gdshaderinc`) - those are `#include`d by other shaders, not
/// standalone resources `vfx()` can load on their own, which is exactly the trap that produced the
/// `vfx("cam", "noise", ...)` hallucination this check catches (the model presumably pattern-matched
/// the *concept* "noise" against a filename it had seen, without checking it was actually loadable).
pub(crate) fn list_known_shader_names(project_dir: &Path) -> Vec<String> {
    let dir = project_dir.join("resources").join("shaders");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("gdshader"))
        .filter_map(|entry| entry.path().file_stem().and_then(|stem| stem.to_str()).map(str::to_string))
        .collect();
    names.sort();
    names
}

/// Mirrors `find_unknown_asset_paths` for `vfx(obj, shader_layer, ...)`'s 2nd argument, which is
/// either a bare shader name, a `[shader_name, layer_id]` pair, or `null` (clears the layer - not
/// a resource reference, skipped). Does not cover `trans`/`trans2`'s own `shader_name` argument
/// yet - same underlying resource, narrower scope for now since `vfx()` is what the reported
/// hallucination actually used.
pub(crate) fn find_unknown_shaders(script: &str, known_shaders: &[String]) -> Vec<AssetPathIssue> {
    let mut issues = Vec::new();
    for call_start in find_call_starts(script, "vfx") {
        let rest = &script[call_start..];
        let Some((_, after_obj)) = extract_quoted_arg(rest) else {
            continue;
        };
        let Some(comma_index) = after_obj.find(',') else {
            continue;
        };
        let second_arg = after_obj[comma_index + 1..].trim_start();
        if second_arg.starts_with("null") {
            continue;
        }
        let array_body = second_arg.strip_prefix('[').map(|rest| rest.trim_start()).unwrap_or(second_arg);
        let Some((shader_name, _)) = extract_quoted_arg(array_body) else {
            continue;
        };
        if known_shaders.iter().any(|known| known == shader_name) {
            continue;
        }
        issues.push(AssetPathIssue {
            written_path: shader_name.to_string(),
            suggested_path: None,
        });
    }
    issues
}

pub fn build_generation_prompt(
    nova2_project_dir: &Path,
    vfx_notes_path: &str,
    target_file: &str,
    existing_content: &str,
    user_prompt: &str,
    atmosphere_plan: Option<&AtmospherePlan>,
) -> Result<String, BackendError> {
    let reference_path = nova2_project_dir.join("novascript-reference.md");
    let reference_text = fs::read_to_string(&reference_path).map_err(|error| {
        BackendError::message(format!("读取 {} 失败: {error}", reference_path.display()))
    })?;

    let mut sections = vec![
        "你现在要为 Nova2 编辑/生成可直接保存为 .txt 的 NovaScript 剧本。严格遵守下面提供的参考文档与约束。输出必须满足以下规则：\n\
        1. 默认只输出脚本文本本身，不要输出解释、标题、Markdown 代码块或围栏。\n\
        2. 如果脚本需要声明新的角色节点，允许在第一行单独输出一行 `#NEWCHARS: [{\"node\":\"...\",\"bind\":\"...\",\"folder\":\"...\"}]`，除此之外不要输出任何额外元信息。\n\
        3. `#NEWCHARS:` 如果出现，必须是整个回复的第一行，后面紧跟合法 JSON 数组。\n\
        4. `#NEWCHARS:` 之后从第二行开始输出完整脚本文本。\n\
        5. 最终脚本必须能被引擎直接重载解析。\n\
        6. 下面“目标文件当前内容”一节如果不为空，必须当成唯一真实状态对待：你的任务是在这份内容基础上只修改用户需求里明确要求的那部分，其余文字（台词、函数调用、角色名、背景/资源路径、label 名等）必须逐字保留，禁止整体重写、禁止臆造新的开场或替换掉没被要求修改的内容。\n\
        7. 只能使用“已知角色绑定名”列表中已经存在的名字，或当前内容/参考文档里已经出现过的名字；不要发明列表之外、读起来很像但不存在的角色名（例如把“二宫”写成别的名字）。确实需要全新角色时才通过 `#NEWCHARS` 声明。\n\
        8. 只有当“目标文件当前内容”一节为空（目标文件不存在或本来就是新文件）时，才允许从零开始生成全新内容。\n\
        9. 默认禁止修改对话台词文本本身（说话人说的话、旁白文字），也不允许新增、删除或合并对话行——除非“用户需求”里明确写出了要修改的具体台词内容，或明确使用了“改台词”“改文本”“改对话”一类的措辞。对话台词以外的部分（背景/灯光/特效 vfx、动画 anim、转场、镜头、音乐音效等函数调用与参数）不受此限制，可以按需修改。\n\
        10. 当用户需求描述的是氛围、情绪、天气、打光、节奏等“演出效果”类诉求（例如“氛围更忧郁一点”“贴合雨天”），必须通过调整或新增 vfx/anim/转场/灯光/环境音等函数调用来实现，绝对不能借此改写台词文字；如果实现该效果确实需要某一行台词配合（极少数情况），必须先确认这正是“用户需求”明确要求的，否则保持台词原样。\n\
        11. `tint`/`env_tint`/`vfx`/`move` 等调用会持久化生效（属性状态不会自动过期），不存在自动“离开场景就还原”的机制。如果本次新增/修改的是这类持久化效果，必须检查“目标文件当前内容”里改动位置之后是否存在切换到不同地点的场景边界（典型信号：之后出现 `trans_fade`/`trans`/`trans_left`/`trans_right`/`trans_up`/`trans_down`，或又一次 `show(\"bg\", 不同路径)` / `show(\"fg\", 不同路径)` 而中间没有先 `hide`/还原）；如果存在这种边界，必须在该边界调用之前补一条还原调用（例如把 `tint` 还原回 `[1,1,1,1]` 或场景原本的色调），避免效果泄漏到下一个不相关的场景。例如：在“学生会办公室”场景加了 `tint(\"bg\", [0.55,0.6,0.68], 0)`，但后面出现了 `trans_fade(\"cam\", func(): show(\"bg\", \"backgrounds/toilet\"), 2)` 切到另一个地点，就必须在这条 `trans_fade` 之前还原 `tint`。如果改动本身已经在文件末尾或后面没有切换到不同地点，则不需要画蛇添足地加还原。同样的道理也适用于场景*内部*的情绪起伏：如果是为了表现冲突升级而逐步加深了某个持久化效果（例如随着对峙升温反复加深 `tint`），在剧情明确出现缓和/转折点（冲突被打断、主角夺回主动权、对方退让等）时，也要让该效果相应地往回收一些，不能任由它只升不降、一路保持到很久之后才在场景末尾一次性清零——效果的强弱要跟着叙事张力的起落走，不是只跟开头/结尾绑定。\n\
        12. 只能引用真实存在的资源路径：“已知背景/立绘资源路径”列表之外、看起来像是合理猜测但实际不存在的路径（例如把 `backgrounds/room` 简写成 `room`）禁止使用；非角色对象（如 `\"bg\"`/`\"fg\"`）的 `show`/`trans*` 图片路径必须原样取自该列表或当前内容里已经出现过的路径。\n\
        13. 同理，`play`/`fade_in` 的曲目名必须原样取自“已知音轨”列表里对应 channel 下的曲目；`sound()` 的音效名必须原样取自“已知音效”列表；`vfx()` 的 shader 名必须原样取自“已知 VFX shader”列表。这几类资源都不允许凭语感/常见叫法臆造（例如想要心跳音效但项目里根本没有对应文件，就不要写 `sound(\"heartbeat\", ...)`；想要噪点效果但项目里只有 `noise.gdshaderinc`〔头文件，不能直接用〕没有独立的 `noise.gdshader`，就不要写 `vfx(\"cam\", \"noise\", ...)`）。如果列表里确实没有合适的现成资源，宁可放弃这个细节或换一种已有资源能实现的方案，也不要编一个不存在的名字。\n\
        14. `entry` 参数是 NovaScript 唯一的“排队/排序”机制：同一个 `<| ... |>` block 里的语句是 GDScript 立即顺序执行的，并不会真的等待——`wait(duration)`/`vfx(...)`/`move(...)` 等返回的是一个可以继续往后链的“链尾”，但只有当你把这个返回值显式接住并传给下一步调用的 `entry` 参数时，下一步才会真的排在“等待结束之后”；如果某一步调用的 `wait(...)` 返回值没有被接住传下去，后面那些仍然用默认 `entry`（`o.anim` 根）的调用会和前面的调用同时触发，而不是按你写的先后顺序播放，等于前面的 `wait` 完全没有效果。需要在同一个 block 内对同一个 obj/层先做 A、停顿、再做 B 时，必须像这样显式链接：`var e = vfx(\"cam\", \"glitch\", 0.9, 0.05)\ne = wait(0.05, e)\ne = vfx(\"cam\", \"glitch\", 0, 0.06, null, e)`（可参考 ch4.txt 的 `hold_entry`/`end_entry` 写法）。另外，`vfx(\"cam\", shader_layer, ...)` 在不指定 `layer_id` 时默认都落在 layer 0：如果同一时间窗口内连续对 `\"cam\"` 调用了两个不同的 shader 名而没有用 `[名字, layer_id]` 区分层，后调用的会直接顶替/覆盖前一个绑定在该层的 shader，不会叠加生效——需要同时叠加多个画面特效时要显式分配不同的 layer_id（0~3）。\n\
        15. 通过 `move`/`show` 的 `coord`（`[x, y, scale, z, angle]`）改变立绘对象的 `scale` 时要格外谨慎：这类构图效果没法在不实际渲染的情况下被准确验证，大幅提高 `scale`（比如从已有的 0.53 跳到 1.05，接近翻倍）很容易把角色的头部/面部推出画面之外。默认应以该对象在“目标文件当前内容”里最近一次 `show`/`move` 设定的 `(x, y, scale)` 作为基准，只做小幅调整（建议相对原 `scale` 的变化幅度控制在 30%~40% 以内），除非用户明确要求大幅推近镜头；如果确实做了较大的 `scale` 调整，必须在你的修改里只做克制的改动，且这类构图改动事后需要用户在预览窗口里确认人物头部是否完整入镜——不要自行假设构图一定正确。\n\
        16. `tint`/`env_tint` 绝对不能把 `\"cam\"` 当作 `obj` 使用：这两个函数对非立绘对象固定走 `modulate` 属性（`tint`）或要求对象有 `CurrentPose`（`env_tint`），而 `\"cam\"` 绑定的是 `Camera3D`，没有 `modulate` 属性——`tint(\"cam\", ...)` 在引擎里会直接抛 `NullReferenceException` 崩掉那个 tween，不是警告或静默失败。`tint`/`env_tint` 只能用在 `\"bg\"`/`\"fg\"`/角色立绘对象上。需要给整个镜头/画面叠加颜色（比如“脸色一变、画面泛红”这类需求）时，必须用 `vfx(\"cam\", [\"color\", layer_id], t, duration, { \"_ColorMul\": ... })`（`layer_id` 选一个当前没被占用的 0~3 层，参考规则 14）。".to_string(),
        format!("## NovaScript 参考文档\n\n{reference_text}"),
    ];

    let known_characters = list_known_character_bind_names(nova2_project_dir);
    if !known_characters.is_empty() {
        sections.push(format!("## 已知角色绑定名\n\n{}", known_characters.join(", ")));
    }

    let known_asset_paths = list_known_asset_script_paths(nova2_project_dir);
    if !known_asset_paths.is_empty() {
        sections.push(format!("## 已知背景/立绘资源路径\n\n{}", known_asset_paths.join(", ")));
    }

    let audio_layout = list_known_audio_layout(nova2_project_dir);
    if !audio_layout.channel_tracks.is_empty() {
        let lines: Vec<String> = audio_layout
            .channel_tracks
            .iter()
            .map(|(channel, tracks)| format!("- {channel}: {}", tracks.join(", ")))
            .collect();
        sections.push(format!("## 已知音轨（play/fade_in 的 channel: 曲目列表）\n\n{}", lines.join("\n")));
    }
    if !audio_layout.one_shot_tracks.is_empty() {
        sections.push(format!("## 已知音效（sound() 可用文件名）\n\n{}", audio_layout.one_shot_tracks.join(", ")));
    }

    let known_shaders = list_known_shader_names(nova2_project_dir);
    if !known_shaders.is_empty() {
        sections.push(format!("## 已知 VFX shader（vfx() 可用名字）\n\n{}", known_shaders.join(", ")));
    }

    if let Some(plan) = atmosphere_plan {
        if !plan.is_empty() {
            let mut plan_lines = Vec::new();
            if !plan.sound.trim().is_empty() {
                plan_lines.push(format!("- 声音：{}", plan.sound.trim()));
            }
            if !plan.text_presentation.trim().is_empty() {
                plan_lines.push(format!("- 文本表现：{}", plan.text_presentation.trim()));
            }
            if !plan.visual_color.trim().is_empty() {
                plan_lines.push(format!("- 画面色彩：{}", plan.visual_color.trim()));
            }
            if !plan.visual_animation.trim().is_empty() {
                plan_lines.push(format!("- 画面动画：{}", plan.visual_animation.trim()));
            }
            if !plan.visual_vfx.trim().is_empty() {
                plan_lines.push(format!("- 画面 VFX：{}", plan.visual_vfx.trim()));
            }
            sections.push(format!(
                "## 本次演出策划\n\n下面是针对本次需求预先做的多维度演出策划，必须把列出的每一项都落实成具体的 NovaScript 函数调用，不能只实现其中一项：\n{}",
                plan_lines.join("\n")
            ));
        }
    }

    let vfx_notes_path = vfx_notes_path.trim();
    if !vfx_notes_path.is_empty() {
        let path = Path::new(vfx_notes_path);
        if path.exists() {
            let vfx_notes = fs::read_to_string(path)
                .map_err(|error| BackendError::message(format!("读取 {} 失败: {error}", path.display())))?;
            sections.push(format!("## 动画/VFX 说明\n\n{vfx_notes}"));
        }
    }

    let existing_content = existing_content.trim();
    sections.push(format!(
        "## 目标文件 {target_file} 当前内容\n\n{}",
        if existing_content.is_empty() {
            "(空 - 这是一个不存在的新文件)".to_string()
        } else {
            existing_content.to_string()
        }
    ));

    sections.push(format!("## 用户需求\n\n{user_prompt}"));
    Ok(sections.join("\n\n"))
}

pub fn parse_generated_output(output: &str) -> Result<(Vec<NewCharacterSpec>, String), BackendError> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return Err(BackendError::message("模型返回了空脚本"));
    }

    if let Some(rest) = trimmed.strip_prefix(NEW_CHARS_PREFIX) {
        let newline_index = rest.find('\n').ok_or_else(|| {
            BackendError::message("#NEWCHARS 后缺少脚本文本，模型输出格式不正确")
        })?;
        let json_text = rest[..newline_index].trim();
        let script = rest[newline_index + 1..].trim().to_string();
        if script.is_empty() {
            return Err(BackendError::message("#NEWCHARS 后的脚本文本为空"));
        }
        let new_chars: Vec<NewCharacterSpec> = serde_json::from_str(json_text)
            .map_err(|error| BackendError::message(format!("解析 #NEWCHARS JSON 失败: {error}")))?;
        return Ok((new_chars, script));
    }

    Ok((Vec::new(), trimmed.to_string()))
}

pub fn register_new_characters(
    project_dir: &Path,
    new_chars: &[NewCharacterSpec],
) -> Result<(), BackendError> {
    for character in new_chars {
        character_template::register_character(
            project_dir,
            &character.node,
            &character.bind,
            &character.folder,
        )?;
    }
    Ok(())
}

pub fn build_retry_prompt(
    base_prompt: &str,
    script: &str,
    error: &PreviewBridgeError,
) -> String {
    let line = error.line.unwrap_or(0);
    let column = error.column.unwrap_or(0);
    format!(
        "{base_prompt}\n\n你上一次生成的脚本如下:\n{script}\n引擎重新加载时报错:第{line}行第{column}列: {}。请修正后重新输出完整脚本；除非确实需要新增角色，否则仍然只输出脚本文本本身；如果仍然需要新增角色，继续按第一行 `#NEWCHARS: [...]`、后续行为完整脚本的格式输出。",
        error.message
    )
}

/// Turns `find_unknown_asset_paths` findings into a `PreviewBridgeError`-shaped message so the
/// caller can feed it straight into the existing `build_retry_prompt` retry loop, without waiting
/// on (or needing) a real engine reload round trip.
pub fn format_asset_path_issues(issues: &[AssetPathIssue]) -> String {
    let details: Vec<String> = issues
        .iter()
        .map(|issue| match &issue.suggested_path {
            Some(suggested) => format!("写了 \"{}\"，但项目里没有这个资源，应该是 \"{suggested}\"", issue.written_path),
            None => format!("写了 \"{}\"，但项目里没有这个资源，请改用“已知背景/立绘资源路径”列表中的真实路径", issue.written_path),
        })
        .collect();
    format!("脚本引用了不存在的资源路径：{}", details.join("；"))
}

pub fn format_audio_track_issues(issues: &[AssetPathIssue]) -> String {
    let details: Vec<String> = issues
        .iter()
        .map(|issue| format!("写了 \"{}\"（格式：channel: track_name），但项目里没有这条音轨，请改用“已知音轨”列表中的真实曲目名", issue.written_path))
        .collect();
    format!("脚本里 play/fade_in 引用了不存在的音轨：{}", details.join("；"))
}

pub fn format_sound_track_issues(issues: &[AssetPathIssue]) -> String {
    let details: Vec<String> = issues
        .iter()
        .map(|issue| format!("写了 \"{}\"，但项目里没有这个音效文件，请改用“已知音效”列表中的真实文件名", issue.written_path))
        .collect();
    format!("脚本里 sound() 引用了不存在的音效：{}", details.join("；"))
}

pub fn format_shader_issues(issues: &[AssetPathIssue]) -> String {
    let details: Vec<String> = issues
        .iter()
        .map(|issue| format!("写了 \"{}\"，但项目里没有这个 shader，请改用“已知 VFX shader”列表中的真实名字", issue.written_path))
        .collect();
    format!("脚本里 vfx() 引用了不存在的 shader：{}", details.join("；"))
}

const ATMOSPHERE_PLAN_SECTIONS: &str = "## 可用的演出函数（节选自 NovaScript 参考文档）\n\n\
- 声音：`play(channel, track_name, vol, duration)`（`channel` 常见 `\"bgm\"`/`\"bgs\"`）、`sound(track_name, vol)`\n\
- 文本表现：`set_box(pos_name, style_name)`、`set_text_appear(mode, char_speed, fade_duration)`、`text_delay(time)`、`box_hide_show(duration, pos_name, style_name)`\n\
- 画面色彩：`tint(obj, color, duration, entry)`、`env_tint(obj, color, duration, entry)`\n\
- 画面动画：`move(obj, coord, scale, angle, duration, entry)`、`wait(duration, entry)`、`anim_hold_begin()`/`anim_hold_end()`、`cam_punch(entry)`\n\
- 画面 VFX：`vfx(obj, shader_layer, t, duration, properties, entry)`";

/// First stage of the two-stage generation pipeline: before writing any NovaScript, ask the model
/// to decompose the requested mood/atmosphere across sound / text-presentation / visual-color /
/// visual-animation / visual-vfx, so the later script-writing stage has an explicit checklist to
/// implement instead of defaulting to a single `tint()` call. Grounded in the real function names
/// from novascript-reference.md so the plan doesn't invent functions that don't exist.
pub fn build_atmosphere_plan_prompt(existing_content: &str, user_prompt: &str) -> String {
    let existing_content = existing_content.trim();
    format!(
        "你是 Nova2 视觉小说的演出策划。针对下面的“用户需求”，从声音、文本表现、画面色彩、画面动画、画面 VFX 五个维度分别思考是否需要用上，以及大致怎么做（用一句中文描述思路和大致会调用的函数，不需要写出完整代码）。哪个维度跟这次需求无关就给空字符串，不要为了凑数硬加。\n\n\
        只输出一个 JSON 对象，字段固定为 sound/text_presentation/visual_color/visual_animation/visual_vfx，值都是字符串，不要输出任何其它文字、解释或 Markdown 代码块围栏。\n\n\
        {ATMOSPHERE_PLAN_SECTIONS}\n\n\
        ## 目标文件当前内容（供你判断场景上下文，不要在这一步直接改它）\n\n{}\n\n\
        ## 用户需求\n\n{user_prompt}",
        if existing_content.is_empty() { "(空)" } else { existing_content }
    )
}

pub fn parse_atmosphere_plan(output: &str) -> Result<AtmospherePlan, BackendError> {
    let trimmed = output.trim();
    let json_text = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(|rest| rest.trim_end_matches("```").trim())
        .unwrap_or(trimmed);
    serde_json::from_str(json_text).map_err(|error| BackendError::message(format!("解析演出策划 JSON 失败: {error}")))
}

pub fn build_atmosphere_plan_retry_prompt(base_prompt: &str, raw_output: &str, error: &BackendError) -> String {
    format!(
        "{base_prompt}\n\n你上一次的输出如下:\n{raw_output}\n解析失败: {error}。请只输出一个合法的 JSON 对象，不要输出任何额外文字或 Markdown 代码块围栏。"
    )
}

/// After a generation attempt is successfully applied, asks the model for a short plain-language
/// changelog entry (not NovaScript) describing what concretely changed, so it can be stored
/// alongside the version-history snapshot and shown to the user without them having to diff raw
/// script text.
pub fn build_summary_prompt(user_prompt: &str, previous_content: &str, final_script: &str) -> String {
    let previous_content = previous_content.trim();
    format!(
        "下面是同一个 NovaScript 剧本文件在一次 AI 修改前后的内容，以及当时的用户需求。请用 1~3 句通俗中文描述这次具体改了什么（可以按声音/文本/画面分别提一句，但只输出自然语言描述本身，不要输出代码、Markdown 列表符号或标题）。如果这次改动包含了下面两类没法仅凭文本确认效果的改动，请在描述末尾单独加一句提醒：(a) 改了某个立绘对象 `move`/`show` 的 `scale`（构图/取景，需要在预览窗口确认人物头部是否完整入镜）；(b) 新增了多步 `vfx`/`anim` 配合 `wait`/`entry` 的时序编排（演出节奏，需要在预览窗口确认动画播放顺序是否符合预期）。如果都不涉及就不用加这句提醒。\n\n\
        ## 用户需求\n\n{user_prompt}\n\n\
        ## 修改前内容\n\n{}\n\n\
        ## 修改后内容\n\n{final_script}",
        if previous_content.is_empty() { "(空 - 这是新文件)" } else { previous_content }
    )
}

pub async fn generate_with_prompt(prompt: &str, api_key: &str, model: &str) -> Result<String, BackendError> {
    llm::generate_script(prompt, api_key, model).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PreviewBridgeError;

    #[test]
    fn parses_plain_script_with_no_new_characters() {
        let (chars, script) = parse_generated_output("show(\"ergong\", \"normal\")\n").unwrap();
        assert!(chars.is_empty());
        assert_eq!(script, "show(\"ergong\", \"normal\")");
    }

    #[test]
    fn parses_newchars_header_and_strips_it_from_script() {
        let output = "#NEWCHARS: [{\"node\":\"Newgirl\",\"bind\":\"newgirl\",\"folder\":\"Newgirl\"}]\nshow(\"newgirl\", \"default\")\n";
        let (chars, script) = parse_generated_output(output).unwrap();
        assert_eq!(chars.len(), 1);
        assert_eq!(chars[0].node, "Newgirl");
        assert_eq!(chars[0].bind, "newgirl");
        assert_eq!(chars[0].folder, "Newgirl");
        assert_eq!(script, "show(\"newgirl\", \"default\")");
    }

    #[test]
    fn rejects_empty_output() {
        assert!(parse_generated_output("   \n  ").is_err());
    }

    #[test]
    fn rejects_newchars_header_with_no_following_script() {
        assert!(parse_generated_output("#NEWCHARS: []").is_err());
    }

    #[test]
    fn rejects_malformed_newchars_json() {
        let output = "#NEWCHARS: not json\nshow(\"ergong\", \"normal\")\n";
        assert!(parse_generated_output(output).is_err());
    }

    #[test]
    fn retry_prompt_includes_previous_script_and_error_location() {
        let error = PreviewBridgeError {
            message: "unexpected token".to_string(),
            line: Some(12),
            column: Some(4),
        };
        let retry = build_retry_prompt("BASE", "show(\"ergong\"", &error);
        assert!(retry.starts_with("BASE"));
        assert!(retry.contains("show(\"ergong\""));
        assert!(retry.contains("第12行第4列"));
        assert!(retry.contains("unexpected token"));
    }

    fn make_fixture_project(existing_script: Option<&str>) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("vvn_generation_test_{}_{}", std::process::id(), rand_suffix()));
        fs::create_dir_all(dir.join("scene")).unwrap();
        fs::create_dir_all(dir.join("resources").join("scenarios")).unwrap();
        fs::write(dir.join("novascript-reference.md"), "# NovaScript reference\nshow(name, pose)\n").unwrap();
        fs::write(
            dir.join("scene").join("game.tscn"),
            "[node name=\"Ergong\" type=\"Node3D\" parent=\"World/Characters\"]\nscript = ExtResource(\"7_cmpsp\")\n_bindName = \"ergong\"\n_imageFolder = \"Ergong\"\n",
        )
        .unwrap();
        if let Some(script) = existing_script {
            fs::write(dir.join("resources").join("scenarios").join("ch1.txt"), script).unwrap();
        }
        dir
    }

    fn rand_suffix() -> u128 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    }

    #[test]
    fn prompt_embeds_existing_content_and_forbids_full_rewrite_when_present() {
        let dir = make_fixture_project(Some("ergong::你好\n"));
        let prompt = build_generation_prompt(&dir, "", "ch1.txt", "ergong::你好\n", "加一句台词", None).unwrap();

        assert!(prompt.contains("ergong::你好"));
        assert!(prompt.contains("已知角色绑定名"));
        assert!(prompt.contains("ergong"));
        assert!(prompt.contains("逐字保留"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prompt_marks_target_as_new_file_when_content_is_empty() {
        let dir = make_fixture_project(None);
        let prompt = build_generation_prompt(&dir, "", "new_chapter.txt", "", "写一个新故事", None).unwrap();

        assert!(prompt.contains("这是一个不存在的新文件"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prompt_forbids_dialogue_edits_unless_explicitly_requested() {
        let dir = make_fixture_project(Some("ergong::你好\n"));
        let prompt = build_generation_prompt(
            &dir,
            "",
            "ch1.txt",
            "ergong::你好\n",
            "让第一章学生会办公室的场景氛围更忧郁一点，贴合雨天",
            None,
        )
        .unwrap();

        assert!(prompt.contains("默认禁止修改对话台词文本本身"));
        assert!(prompt.contains("绝对不能借此改写台词文字"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prompt_includes_atmosphere_plan_when_present() {
        let dir = make_fixture_project(Some("ergong::你好\n"));
        let plan = AtmospherePlan {
            sound: "渐入雨声环境音".to_string(),
            text_presentation: String::new(),
            visual_color: "把 bg 染成偏冷的蓝灰色".to_string(),
            visual_animation: String::new(),
            visual_vfx: String::new(),
        };
        let prompt = build_generation_prompt(&dir, "", "ch1.txt", "ergong::你好\n", "氛围更忧郁一点", Some(&plan)).unwrap();

        assert!(prompt.contains("本次演出策划"));
        assert!(prompt.contains("渐入雨声环境音"));
        assert!(prompt.contains("把 bg 染成偏冷的蓝灰色"));
        assert!(!prompt.contains("- 文本表现："));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_atmosphere_plan_adds_no_section() {
        let dir = make_fixture_project(Some("ergong::你好\n"));
        let plan = AtmospherePlan::default();
        let prompt = build_generation_prompt(&dir, "", "ch1.txt", "ergong::你好\n", "加一句台词", Some(&plan)).unwrap();

        assert!(!prompt.contains("本次演出策划"));

        fs::remove_dir_all(&dir).ok();
    }

    fn write_background_fixture(dir: &std::path::Path, name: &str) {
        let backgrounds_dir = dir.join("resources").join("backgrounds");
        fs::create_dir_all(&backgrounds_dir).unwrap();
        fs::write(backgrounds_dir.join(format!("{name}.png")), b"fake-png").unwrap();
    }

    #[test]
    fn lists_known_asset_script_paths_without_resources_prefix_or_extension() {
        let dir = make_fixture_project(None);
        write_background_fixture(&dir, "room");

        let paths = list_known_asset_script_paths(&dir);
        assert_eq!(paths, vec!["backgrounds/room".to_string()]);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn flags_hallucinated_path_missing_known_prefix() {
        let known_paths = vec!["backgrounds/room".to_string()];
        let issues = find_unknown_asset_paths("show(\"bg\", \"room\")", &known_paths, &[]);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].written_path, "room");
        assert_eq!(issues[0].suggested_path, Some("backgrounds/room".to_string()));
    }

    #[test]
    fn accepts_correct_known_path() {
        let known_paths = vec!["backgrounds/room".to_string()];
        let issues = find_unknown_asset_paths("show(\"bg\", \"backgrounds/room\")", &known_paths, &[]);
        assert!(issues.is_empty());
    }

    #[test]
    fn skips_character_pose_argument() {
        let known_paths = vec!["backgrounds/room".to_string()];
        let known_characters = vec!["ergong".to_string()];
        let issues = find_unknown_asset_paths("show(\"ergong\", \"normal\")", &known_paths, &known_characters);
        assert!(issues.is_empty());
    }

    #[test]
    fn skips_camera_callable_transition() {
        let known_paths = vec!["backgrounds/room".to_string()];
        let issues = find_unknown_asset_paths(
            "trans_fade(\"cam\", func(): show(\"bg\", \"backgrounds/room\"), 2)",
            &known_paths,
            &[],
        );
        assert!(issues.is_empty());
    }

    #[test]
    fn flags_unknown_path_with_no_suggestion_when_no_prefix_matches() {
        let known_paths = vec!["backgrounds/room".to_string()];
        let issues = find_unknown_asset_paths("show(\"bg\", \"nonexistent\")", &known_paths, &[]);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].suggested_path, None);
    }

    #[test]
    fn call_starts_do_not_match_inside_longer_identifier() {
        // Regression: a naive substring search for "play(" also matches inside "video_play(" -
        // this would misreport video_play()'s first arg (a duration, not a channel name) as an
        // unknown audio channel/track.
        let starts = find_call_starts("video_play(2)\nplay(\"bgm\", \"prelude\")", "play");
        assert_eq!(starts.len(), 1);
        assert!(&"video_play(2)\nplay(\"bgm\", \"prelude\")"[starts[0]..].starts_with("\"bgm\""));
    }

    fn write_audio_fixture(dir: &std::path::Path, folder: &str, track_name: &str) {
        let audio_dir = dir.join("resources").join("audio").join(folder);
        fs::create_dir_all(&audio_dir).unwrap();
        fs::write(audio_dir.join(format!("{track_name}.ogg")), b"fake-ogg").unwrap();
    }

    fn write_game_tscn_with_audio_channels(dir: &std::path::Path) {
        fs::create_dir_all(dir.join("scene")).unwrap();
        fs::write(
            dir.join("scene").join("game.tscn"),
            "[node name=\"Bgm\" type=\"Node\" parent=\"Audio\"]\n\
            _bindName = \"bgm\"\n\
            _audioFolder = \"bgm\"\n\
            \n\
            [node name=\"Bgs\" type=\"Node\" parent=\"Audio\"]\n\
            _bindName = \"bgs\"\n\
            _audioFolder = \"sound\"\n\
            \n\
            [node name=\"Sound\" type=\"Node\" parent=\"Audio\"]\n\
            _audioFolder = \"sound\"\n",
        )
        .unwrap();
    }

    #[test]
    fn lists_known_audio_layout_from_game_tscn() {
        let dir = std::env::temp_dir().join(format!("vvn_audio_layout_test_{}_{}", std::process::id(), rand_suffix()));
        write_game_tscn_with_audio_channels(&dir);
        write_audio_fixture(&dir, "bgm", "prelude");
        write_audio_fixture(&dir, "sound", "rain");
        write_audio_fixture(&dir, "sound", "flap");

        let layout = list_known_audio_layout(&dir);
        let bgm_tracks = layout.channel_tracks.iter().find(|(name, _)| name == "bgm").map(|(_, tracks)| tracks.clone());
        let bgs_tracks = layout.channel_tracks.iter().find(|(name, _)| name == "bgs").map(|(_, tracks)| tracks.clone());
        assert_eq!(bgm_tracks, Some(vec!["prelude".to_string()]));
        assert_eq!(bgs_tracks, Some(vec!["flap".to_string(), "rain".to_string()]));
        assert_eq!(layout.one_shot_tracks, vec!["flap".to_string(), "rain".to_string()]);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn flags_hallucinated_audio_track() {
        let layout = AudioLayout {
            channel_tracks: vec![("bgm".to_string(), vec!["prelude".to_string()])],
            one_shot_tracks: Vec::new(),
        };
        let issues = find_unknown_audio_tracks("play(\"bgm\", \"nonexistent\")", &layout);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].written_path, "bgm: nonexistent");
    }

    #[test]
    fn accepts_known_audio_track() {
        let layout = AudioLayout {
            channel_tracks: vec![("bgm".to_string(), vec!["prelude".to_string()])],
            one_shot_tracks: Vec::new(),
        };
        let issues = find_unknown_audio_tracks("play(\"bgm\", \"prelude\")", &layout);
        assert!(issues.is_empty());
    }

    #[test]
    fn ignores_unknown_channel_for_audio_track_check() {
        let layout = AudioLayout {
            channel_tracks: vec![("bgm".to_string(), vec!["prelude".to_string()])],
            one_shot_tracks: Vec::new(),
        };
        let issues = find_unknown_audio_tracks("play(\"voice\", \"whatever\")", &layout);
        assert!(issues.is_empty());
    }

    #[test]
    fn flags_hallucinated_sound_effect() {
        let known = vec!["flap".to_string(), "rain".to_string()];
        let issues = find_unknown_sound_tracks("sound(\"heartbeat\", 0.7)", &known);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].written_path, "heartbeat");
    }

    #[test]
    fn accepts_known_sound_effect() {
        let known = vec!["flap".to_string(), "rain".to_string()];
        let issues = find_unknown_sound_tracks("sound(\"flap\", 0.5)", &known);
        assert!(issues.is_empty());
    }

    #[test]
    fn lists_known_shader_names_excluding_gdshaderinc() {
        let dir = std::env::temp_dir().join(format!("vvn_shader_list_test_{}_{}", std::process::id(), rand_suffix()));
        let shaders_dir = dir.join("resources").join("shaders");
        fs::create_dir_all(&shaders_dir).unwrap();
        fs::write(shaders_dir.join("mono.gdshader"), b"shader").unwrap();
        fs::write(shaders_dir.join("noise.gdshaderinc"), b"include").unwrap();

        let names = list_known_shader_names(&dir);
        assert_eq!(names, vec!["mono".to_string()]);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn flags_hallucinated_shader() {
        let known = vec!["mono".to_string()];
        let issues = find_unknown_shaders("vfx(\"cam\", \"noise\", 0.5, 0.5)", &known);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].written_path, "noise");
    }

    #[test]
    fn accepts_known_shader_in_array_form() {
        let known = vec!["mono".to_string()];
        let issues = find_unknown_shaders("vfx(\"cam\", [\"mono\", 1], 1, 1)", &known);
        assert!(issues.is_empty());
    }

    #[test]
    fn skips_null_shader_clear_call() {
        let known = vec!["mono".to_string()];
        let issues = find_unknown_shaders("vfx(\"cam\", null)", &known);
        assert!(issues.is_empty());
    }

    #[test]
    fn parses_atmosphere_plan_json() {
        let output = r#"{"sound":"渐入雨声","text_presentation":"","visual_color":"调暗色调","visual_animation":"","visual_vfx":""}"#;
        let plan = parse_atmosphere_plan(output).unwrap();
        assert_eq!(plan.sound, "渐入雨声");
        assert_eq!(plan.visual_color, "调暗色调");
        assert_eq!(plan.text_presentation, "");
    }

    #[test]
    fn parses_atmosphere_plan_json_wrapped_in_markdown_fence() {
        let output = "```json\n{\"sound\":\"渐入雨声\",\"text_presentation\":\"\",\"visual_color\":\"\",\"visual_animation\":\"\",\"visual_vfx\":\"\"}\n```";
        let plan = parse_atmosphere_plan(output).unwrap();
        assert_eq!(plan.sound, "渐入雨声");
    }

    #[test]
    fn rejects_malformed_atmosphere_plan_json() {
        assert!(parse_atmosphere_plan("not json").is_err());
    }

    #[test]
    fn summary_prompt_embeds_before_after_and_user_request() {
        let prompt = build_summary_prompt("氛围更忧郁一点", "tint(\"bg\", [1,1,1], 0)", "tint(\"bg\", [0.5,0.6,0.7], 0)");
        assert!(prompt.contains("氛围更忧郁一点"));
        assert!(prompt.contains("tint(\"bg\", [1,1,1], 0)"));
        assert!(prompt.contains("tint(\"bg\", [0.5,0.6,0.7], 0)"));
    }

    #[test]
    fn summary_prompt_asks_for_visual_review_flag() {
        let prompt = build_summary_prompt("a", "b", "c");
        assert!(prompt.contains("scale"));
        assert!(prompt.contains("entry"));
    }

    #[test]
    fn prompt_includes_entry_chaining_and_scale_caution_rules() {
        let dir = make_fixture_project(Some("ergong::你好\n"));
        let prompt = build_generation_prompt(&dir, "", "ch1.txt", "ergong::你好\n", "加一个画面故障效果", None).unwrap();

        assert!(prompt.contains("entry"));
        assert!(prompt.contains("hold_entry"));
        assert!(prompt.contains("layer_id"));
        assert!(prompt.contains("头部"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prompt_extends_scope_leakage_rule_to_narrative_easing() {
        let dir = make_fixture_project(Some("ergong::你好\n"));
        let prompt = build_generation_prompt(&dir, "", "ch1.txt", "ergong::你好\n", "对峙更紧张", None).unwrap();

        assert!(prompt.contains("缓和/转折点"));
        assert!(prompt.contains("只升不降"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prompt_forbids_tint_on_cam() {
        let dir = make_fixture_project(Some("ergong::你好\n"));
        let prompt = build_generation_prompt(&dir, "", "ch1.txt", "ergong::你好\n", "脸色一变，画面泛红", None).unwrap();

        assert!(prompt.contains("tint(\"cam\""));
        assert!(prompt.contains("NullReferenceException"));
        assert!(prompt.contains("_ColorMul"));

        fs::remove_dir_all(&dir).ok();
    }

    // Manual verification only, never run automatically: feeds the real nova2 ch1.txt content
    // (read-only - nothing gets written back) and the exact user request that previously caused a
    // full hallucinated rewrite, to inspect whether the fixed prompt now produces a targeted edit.
    // Run with: VVN_SMOKE_DEEPSEEK_KEY=... VVN_SMOKE_PROJECT_DIR=E:\nova2\Nova2 cargo test
    //   prompt_fix_manual_check -- --nocapture --ignored
    #[tokio::test]
    #[ignore]
    async fn prompt_fix_manual_check() {
        let project_dir_str = std::env::var("VVN_SMOKE_PROJECT_DIR").expect("set VVN_SMOKE_PROJECT_DIR");
        let project_dir = Path::new(&project_dir_str);
        let deepseek_key = std::env::var("VVN_SMOKE_DEEPSEEK_KEY").expect("set VVN_SMOKE_DEEPSEEK_KEY");

        let existing = fs::read_to_string(project_dir.join("resources").join("scenarios").join("ch1.txt")).unwrap();
        let prompt = build_generation_prompt(
            project_dir,
            "",
            "ch1.txt",
            &existing,
            "第一章王二宫的第一句话开始加上水波向外的vfx，直到张浅野出现",
            None,
        )
        .unwrap();

        println!("=== prompt sent ===\n{prompt}\n=== end prompt ===");
        let output = generate_with_prompt(&prompt, &deepseek_key, llm::DEEPSEEK_MODEL_FLASH).await.unwrap();
        println!("=== model output ===\n{output}\n=== end ===");
    }
}
