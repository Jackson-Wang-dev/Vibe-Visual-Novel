use crate::{character_template, llm, world_state, BackendError, PreviewBridgeError};
use serde::Deserialize;
use std::{collections::HashMap, fs, path::Path};

#[cfg(test)]
const NEW_CHARS_PREFIX: &str = "#NEWCHARS:";
const ASSET_ROOTS: [&str; 1] = ["backgrounds"];
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

/// Scans all `.txt` scenario files for speaker display names (both `DisplayName::dialogue` prefixes
/// and `auto_voice_on("DisplayName", ...)` call arguments). Used alongside
/// `list_known_character_bind_names` to populate `GroundingBundle.known_characters` with display
/// names so that the regularize confirmation view doesn't flag existing speakers as new characters.
pub(crate) fn list_known_speaker_display_names(project_dir: &Path) -> Vec<String> {
    let scenarios_dir = project_dir.join("resources").join("scenarios");
    let Ok(entries) = fs::read_dir(&scenarios_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("txt") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty()
                || trimmed.starts_with('@')
                || trimmed.starts_with('<')
                || trimmed.starts_with('#')
                || trimmed.starts_with("//")
            {
                continue;
            }
            // Speaker prefix: DisplayName::dialogue
            if let Some(sep) = trimmed.find("::") {
                let candidate = trimmed[..sep].trim();
                if !candidate.is_empty() && !names.iter().any(|n| n == candidate) {
                    names.push(candidate.to_string());
                }
            }
            // auto_voice_on("DisplayName", ...)
            for call_start in find_call_starts(trimmed, "auto_voice_on") {
                if let Some((display_name, _)) = extract_two_quoted_args(&trimmed[call_start..]) {
                    if !display_name.is_empty() && !names.iter().any(|n| n == display_name) {
                        names.push(display_name.to_string());
                    }
                }
            }
        }
    }
    names
}

/// Parses `character.gd`'s static `poses` dictionary to extract valid pose aliases per character.
/// Returns `(bind_name, [pose_alias, ...])` pairs. Pose aliases are the only valid second argument
/// to `show(charname, ...)` for composite-sprite character objects — raw part paths like
/// `"standings/Xiben/body"` are internal engine details, not script-level identifiers.
pub(crate) fn list_known_character_poses(project_dir: &Path) -> Vec<(String, Vec<String>)> {
    let gd_path = project_dir
        .join("nova")
        .join("sources")
        .join("gdscript")
        .join("runtime")
        .join("character.gd");
    let Ok(content) = fs::read_to_string(&gd_path) else {
        return Vec::new();
    };
    let Some(dict_start) = content.find("static var poses: Dictionary = {") else {
        return Vec::new();
    };
    let mut result: Vec<(String, Vec<String>)> = Vec::new();
    let mut current_char: Option<String> = None;
    let mut current_poses: Vec<String> = Vec::new();
    let mut in_outer_dict = false;
    let mut in_inner_dict = false;
    for line in content[dict_start..].lines() {
        let trimmed = line.trim();
        if !in_outer_dict {
            in_outer_dict = true;
            continue;
        }
        if in_inner_dict {
            if trimmed.starts_with('}') {
                in_inner_dict = false;
                if let Some(name) = current_char.take() {
                    result.push((name, std::mem::take(&mut current_poses)));
                }
            } else if let Some(eq_pos) = trimmed.find(" = \"") {
                let pose = trimmed[..eq_pos].trim();
                if !pose.is_empty() {
                    current_poses.push(pose.to_string());
                }
            }
        } else if trimmed.starts_with('}') {
            break;
        } else if let Some(eq_pos) = trimmed.rfind(" = {") {
            let name = trimmed[..eq_pos].trim();
            if !name.is_empty() {
                current_char = Some(name.to_string());
                current_poses = Vec::new();
                in_inner_dict = true;
            }
        }
    }
    result
}

/// Walks `resources/backgrounds` and converts each image file into the path string NovaScript
/// expects in a `show`/`trans*` call: relative to `resources/`, no extension, forward slashes
/// (e.g. `resources/backgrounds/room.png` -> `backgrounds/room`). Used to ground the generation
/// prompt with real paths and to catch hallucinated paths before spending a Godot reload round trip.
/// Note: `standings/` is intentionally excluded — character sprites are referenced by pose alias
/// (see `list_known_character_poses`), not by raw part path, so listing them here would mislead
/// the model into using paths as pose arguments.
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

#[derive(Debug, Clone, PartialEq)]
pub struct CharacterPoseIssue {
    pub character: String,
    pub written_pose: String,
    pub valid_poses: Vec<String>,
}

/// Checks that every `show(charname, pose, ...)` call where `charname` is a known character uses
/// a valid pose alias from `character.gd`'s `poses` dictionary, or a literal composite-sprite
/// string containing `+` (which `character.gd`'s `get_pose` passes through directly without lookup).
/// Flags calls that use raw `standings/` paths or unrecognized alias names.
pub(crate) fn find_unknown_character_poses(
    script: &str,
    character_poses: &[(String, Vec<String>)],
) -> Vec<CharacterPoseIssue> {
    let mut issues = Vec::new();
    for call_start in find_call_starts(script, "show") {
        let Some((obj, pose_arg)) = extract_call_args(&script[call_start..], "show") else {
            continue;
        };
        let Some((_, valid_poses)) = character_poses.iter().find(|(name, _)| name == obj) else {
            continue;
        };
        if pose_arg.contains('+') {
            continue;
        }
        if valid_poses.iter().any(|p| p == pose_arg) {
            continue;
        }
        issues.push(CharacterPoseIssue {
            character: obj.to_string(),
            written_pose: pose_arg.to_string(),
            valid_poses: valid_poses.clone(),
        });
    }
    issues
}

pub fn format_character_pose_issues(issues: &[CharacterPoseIssue]) -> String {
    let details: Vec<String> = issues
        .iter()
        .map(|issue| {
            let valid = issue.valid_poses.join("/");
            format!(
                "show(\"{}\", \"{}\", ...) 用了无效的 pose 别名（路径或不存在的别名），该角色合法的 pose 别名是：{}",
                issue.character, issue.written_pose, valid
            )
        })
        .collect();
    format!(
        "发现角色立绘 pose 别名错误（show 第二个参数必须是 pose 别名，不能是 standings/ 路径）：\n{}",
        details.join("\n")
    )
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

const SCENE_BOUNDARY_CALLS: [&str; 7] = ["trans_fade", "trans_left", "trans_right", "trans_up", "trans_down", "trans2", "trans"];
const AUDIO_OPEN_CALLS: [&str; 2] = ["play", "fade_in"];
const AUDIO_CLOSE_CALLS: [&str; 2] = ["stop", "fade_out"];

/// Finds every occurrence of `marker` as a literal substring in `script`, returning the byte
/// offset of the start of each match - for the `# vvn:seq begin`/`# vvn:seq end` sentinel comments
/// `build_node_skeleton` emits, which aren't function calls, so `find_call_starts`' identifier-
/// boundary guard doesn't apply and isn't needed (these are whole-line sentinels only AUTOSTAGE's
/// own skeleton builder emits, not user-writable identifiers that could collide).
fn find_marker_lines(script: &str, marker: &str) -> Vec<usize> {
    let mut offsets = Vec::new();
    let mut search_from = 0;
    while let Some(found) = script[search_from..].find(marker) {
        offsets.push(search_from + found);
        search_from += found + marker.len();
    }
    offsets
}

/// What kind of persistent state a `LifecycleIssue` is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleCallKind {
    Tint,
    EnvTint,
    Vfx,
    Audio,
    Show,
}

/// One piece of state that was still "open" (shown/tinted/playing/etc) at a scene boundary (or
/// EOF) without being closed/reverted first - the exact failure mode rule 11's prose asks the
/// model to self-police and consistently fails to. `auto_fix` is `Some(line to insert)` for the
/// mechanical cases (tint/env_tint/vfx/audio all have one unambiguous "closed" value); it's `None`
/// for `Show`, since whether a character/object should still be on screen in the next scene is a
/// content judgment, not something code should silently decide.
#[derive(Debug, Clone, PartialEq)]
pub struct LifecycleIssue {
    pub object: String,
    pub call_kind: LifecycleCallKind,
    pub opened_at_line: u32,
    pub boundary_line: u32,
    pub boundary_kind: &'static str,
    pub auto_fix: Option<String>,
}

enum LifecycleEvent {
    Show { obj: String, is_bg_or_fg: bool, path: Option<String> },
    Hide { obj: String },
    Tint { obj: String, is_default: bool },
    EnvTint { obj: String, is_default: bool },
    Vfx { obj: String, layer: u32, is_clear: bool },
    AudioOpen { channel: String },
    AudioClose { channel: String },
    Boundary { kind: &'static str },
    /// A `# vvn:seq begin`/`# vvn:seq end` sentinel comment, emitted only by `build_node_skeleton`
    /// around a `#seq:`-marked performance-sequence span's raw passthrough content. While inside
    /// such a span, ordinary scene-boundary detection (trans* calls, bg/fg path switches) is
    /// suppressed entirely - rapid background switching IS the performance there, not a leak - and
    /// the span's end is the sole cleanup boundary.
    SequenceBoundary { is_start: bool },
}

fn byte_offset_to_line(script: &str, offset: usize) -> u32 {
    let clamped = offset.min(script.len());
    script[..clamped].matches('\n').count() as u32 + 1
}

/// `vfx("cam", ...)` has 4 independent layers (0-3, see novascript-reference.md's VFX layer
/// section); every other target has a single slot, always layer 0. Layer-qualifying only `"cam"`
/// in the reported object name keeps single-slot objects' messages simple while still letting two
/// different `"cam"` layers be reported as the distinct issues they are.
fn vfx_object_label(obj: &str, layer: u32) -> String {
    if obj == "cam" {
        format!("{obj}:{layer}")
    } else {
        obj.to_string()
    }
}

/// `tint`/`env_tint`'s neutral/reverted color is white, fully opaque - `[1,1,1,1]` - but the
/// `color` argument's shorthand forms (scalar, `[gray]`, `[gray,alpha]`, `[r,g,b]`, `[r,g,b,a]`)
/// mean a literal "is this exactly `[1,1,1,1]`" string compare would miss `1`, `[1,1,1]`, etc.
/// Treats any argument that, once brackets are stripped, is a comma-separated list of nothing but
/// `1`/`1.0`/`1.00` tokens as "already reverted to default".
fn looks_like_default_color(color_arg: &str) -> bool {
    let cleaned = color_arg.trim().trim_start_matches('[').trim_end_matches(']');
    if cleaned.is_empty() {
        return false;
    }
    cleaned.split(',').all(|token| matches!(token.trim(), "1" | "1.0" | "1.00"))
}

/// Like `extract_call_args`, but for calls whose 2nd positional argument is an arbitrary
/// expression (e.g. `tint`/`env_tint`'s `[r,g,b,a]` color array) rather than a quoted string.
/// Tracks bracket/paren depth so a comma inside `[...]` isn't mistaken for the argument separator.
fn extract_color_arg(rest: &str) -> Option<(&str, &str)> {
    let (obj, after_obj) = extract_quoted_arg(rest)?;
    let after_comma = after_obj.trim_start().strip_prefix(',')?.trim_start();
    let mut depth: i32 = 0;
    for (index, ch) in after_comma.char_indices() {
        match ch {
            '[' | '(' => depth += 1,
            ']' | ')' if depth > 0 => depth -= 1,
            ')' if depth == 0 => return Some((obj, after_comma[..index].trim())),
            ',' if depth == 0 => return Some((obj, after_comma[..index].trim())),
            _ => {}
        }
    }
    None
}

fn parse_layer_id(rest: &str) -> Option<u32> {
    let after_comma = rest.trim_start().strip_prefix(',')?;
    let digits: String = after_comma.trim_start().chars().take_while(|ch| ch.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Parses `vfx(obj, shader_layer, ...)`'s 2nd argument into `(shader_name_or_None_if_clearing,
/// layer_id)`. `shader_layer` can be a bare shader name (layer 0), `[shader_name, layer_id]`,
/// `null` (clears layer 0), or `[null, layer_id]` (clears that specific cam layer).
fn extract_vfx_args(rest: &str) -> Option<(&str, Option<&str>, u32)> {
    let (obj, after_obj) = extract_quoted_arg(rest)?;
    let after_comma = after_obj.trim_start().strip_prefix(',')?.trim_start();
    if let Some(array_rest) = after_comma.strip_prefix('[') {
        let array_rest = array_rest.trim_start();
        if let Some(after_null) = array_rest.strip_prefix("null") {
            return Some((obj, None, parse_layer_id(after_null).unwrap_or(0)));
        }
        let (shader_name, after_shader) = extract_quoted_arg(array_rest)?;
        return Some((obj, Some(shader_name), parse_layer_id(after_shader).unwrap_or(0)));
    }
    if after_comma.starts_with("null") {
        return Some((obj, None, 0));
    }
    let (shader_name, _) = extract_quoted_arg(after_comma)?;
    Some((obj, Some(shader_name), 0))
}

fn collect_lifecycle_events(script: &str) -> Vec<(usize, LifecycleEvent)> {
    let mut events: Vec<(usize, LifecycleEvent)> = Vec::new();

    for call_start in find_call_starts(script, "show") {
        if let Some((obj, path)) = extract_call_args(&script[call_start..], "show") {
            let is_bg_or_fg = obj == "bg" || obj == "fg";
            events.push((call_start, LifecycleEvent::Show { obj: obj.to_string(), is_bg_or_fg, path: Some(path.to_string()) }));
        }
    }
    for call_start in find_call_starts(script, "hide") {
        if let Some((obj, _)) = extract_quoted_arg(&script[call_start..]) {
            events.push((call_start, LifecycleEvent::Hide { obj: obj.to_string() }));
        }
    }
    for call_start in find_call_starts(script, "tint") {
        if let Some((obj, color)) = extract_color_arg(&script[call_start..]) {
            events.push((call_start, LifecycleEvent::Tint { obj: obj.to_string(), is_default: looks_like_default_color(color) }));
        }
    }
    for call_start in find_call_starts(script, "env_tint") {
        if let Some((obj, color)) = extract_color_arg(&script[call_start..]) {
            events.push((call_start, LifecycleEvent::EnvTint { obj: obj.to_string(), is_default: looks_like_default_color(color) }));
        }
    }
    for call_start in find_call_starts(script, "vfx") {
        if let Some((obj, shader, layer)) = extract_vfx_args(&script[call_start..]) {
            events.push((call_start, LifecycleEvent::Vfx { obj: obj.to_string(), layer, is_clear: shader.is_none() }));
        }
    }
    for call_name in AUDIO_OPEN_CALLS {
        for call_start in find_call_starts(script, call_name) {
            if let Some((channel, _)) = extract_two_quoted_args(&script[call_start..]) {
                events.push((call_start, LifecycleEvent::AudioOpen { channel: channel.to_string() }));
            }
        }
    }
    for call_name in AUDIO_CLOSE_CALLS {
        for call_start in find_call_starts(script, call_name) {
            if let Some((channel, _)) = extract_quoted_arg(&script[call_start..]) {
                events.push((call_start, LifecycleEvent::AudioClose { channel: channel.to_string() }));
            }
        }
    }
    for call_name in SCENE_BOUNDARY_CALLS {
        for call_start in find_call_starts(script, call_name) {
            events.push((call_start, LifecycleEvent::Boundary { kind: call_name }));
        }
    }
    for offset in find_marker_lines(script, world_state::SEQUENCE_BEGIN_MARKER) {
        events.push((offset, LifecycleEvent::SequenceBoundary { is_start: true }));
    }
    for offset in find_marker_lines(script, world_state::SEQUENCE_END_MARKER) {
        events.push((offset, LifecycleEvent::SequenceBoundary { is_start: false }));
    }

    events.sort_by_key(|(offset, _)| *offset);
    events
}

/// Emits a `LifecycleIssue` for everything still open at this boundary, then drains the
/// mechanical maps (tint/env_tint/vfx/audio - the boundary is assumed fixed going forward, whether
/// by auto-fix or by the model acting on feedback) while leaving `show_open` untouched, since
/// "is this object still supposed to be on screen" needs re-evaluating at every boundary it's
/// still open at, not just the first one.
fn flush_lifecycle_boundary(
    line: u32,
    kind: &'static str,
    tint_open: &mut HashMap<String, u32>,
    env_tint_open: &mut HashMap<String, u32>,
    vfx_open: &mut HashMap<(String, u32), u32>,
    audio_open: &mut HashMap<String, u32>,
    show_open: &HashMap<String, u32>,
    issues: &mut Vec<LifecycleIssue>,
) {
    for (obj, opened_at_line) in tint_open.drain() {
        issues.push(LifecycleIssue {
            auto_fix: Some(format!("tint(\"{obj}\", [1,1,1,1], 0)")),
            object: obj,
            call_kind: LifecycleCallKind::Tint,
            opened_at_line,
            boundary_line: line,
            boundary_kind: kind,
        });
    }
    for (obj, opened_at_line) in env_tint_open.drain() {
        issues.push(LifecycleIssue {
            auto_fix: Some(format!("env_tint(\"{obj}\", [1,1,1,1], 0)")),
            object: obj,
            call_kind: LifecycleCallKind::EnvTint,
            opened_at_line,
            boundary_line: line,
            boundary_kind: kind,
        });
    }
    for ((obj, layer), opened_at_line) in vfx_open.drain() {
        let auto_fix = if obj == "cam" {
            Some(format!("vfx(\"{obj}\", [null, {layer}])"))
        } else {
            Some(format!("vfx(\"{obj}\", null)"))
        };
        issues.push(LifecycleIssue {
            object: vfx_object_label(&obj, layer),
            call_kind: LifecycleCallKind::Vfx,
            opened_at_line,
            boundary_line: line,
            boundary_kind: kind,
            auto_fix,
        });
    }
    for (channel, opened_at_line) in audio_open.drain() {
        issues.push(LifecycleIssue {
            auto_fix: Some(format!("stop(\"{channel}\", 0)")),
            object: channel,
            call_kind: LifecycleCallKind::Audio,
            opened_at_line,
            boundary_line: line,
            boundary_kind: kind,
        });
    }
    for (obj, opened_at_line) in show_open {
        issues.push(LifecycleIssue {
            object: obj.clone(),
            call_kind: LifecycleCallKind::Show,
            opened_at_line: *opened_at_line,
            boundary_line: line,
            boundary_kind: kind,
            auto_fix: None,
        });
    }
}

/// Walks the script top-to-bottom tracking what visual/audio state is "open" (shown, non-default
/// tint, an active vfx layer, a playing audio channel), and flags anything still open at a scene
/// boundary or EOF. Scene boundaries reuse rule 11's exact definition: a `trans*` call, or a
/// repeated `show("bg"|"fg", different_path)` with no intervening `hide`. Same-layer `vfx`
/// overwrites and `"cam"`'s 4 independent layers are handled per novascript-reference.md's VFX
/// layer section, not flagged as leaks. `move` is intentionally not tracked - it has no binary
/// open/close state, only content-dependent "the right value", and isn't a leak in the same sense.
pub(crate) fn check_lifecycle_issues(script: &str) -> Vec<LifecycleIssue> {
    let events = collect_lifecycle_events(script);

    let mut tint_open: HashMap<String, u32> = HashMap::new();
    let mut env_tint_open: HashMap<String, u32> = HashMap::new();
    let mut vfx_open: HashMap<(String, u32), u32> = HashMap::new();
    let mut audio_open: HashMap<String, u32> = HashMap::new();
    let mut show_open: HashMap<String, u32> = HashMap::new();
    let mut last_bg_fg_path: HashMap<String, String> = HashMap::new();
    let mut issues: Vec<LifecycleIssue> = Vec::new();
    let mut in_sequence = false;

    for (offset, event) in events {
        let line = byte_offset_to_line(script, offset);
        match event {
            LifecycleEvent::SequenceBoundary { is_start: true } => {
                in_sequence = true;
            }
            LifecycleEvent::SequenceBoundary { is_start: false } => {
                flush_lifecycle_boundary(line, "sequence_end", &mut tint_open, &mut env_tint_open, &mut vfx_open, &mut audio_open, &show_open, &mut issues);
                in_sequence = false;
            }
            LifecycleEvent::Boundary { kind } => {
                if !in_sequence {
                    flush_lifecycle_boundary(line, kind, &mut tint_open, &mut env_tint_open, &mut vfx_open, &mut audio_open, &show_open, &mut issues);
                }
            }
            LifecycleEvent::Show { obj, is_bg_or_fg, path } => {
                if is_bg_or_fg {
                    let switched = last_bg_fg_path.get(&obj).is_some_and(|prev| Some(prev.as_str()) != path.as_deref());
                    if switched && !in_sequence {
                        flush_lifecycle_boundary(line, "show_switch", &mut tint_open, &mut env_tint_open, &mut vfx_open, &mut audio_open, &show_open, &mut issues);
                    }
                    // Tracking continues even while suppressed inside a sequence - otherwise the
                    // first real switch right after the sequence ends would misjudge whether it's
                    // actually different from whatever was showing before the sequence began.
                    if let Some(path) = path {
                        last_bg_fg_path.insert(obj, path);
                    }
                } else {
                    show_open.entry(obj).or_insert(line);
                }
            }
            LifecycleEvent::Hide { obj } => {
                show_open.remove(&obj);
                last_bg_fg_path.remove(&obj);
            }
            LifecycleEvent::Tint { obj, is_default } => {
                if is_default {
                    tint_open.remove(&obj);
                } else {
                    tint_open.entry(obj).or_insert(line);
                }
            }
            LifecycleEvent::EnvTint { obj, is_default } => {
                if is_default {
                    env_tint_open.remove(&obj);
                } else {
                    env_tint_open.entry(obj).or_insert(line);
                }
            }
            LifecycleEvent::Vfx { obj, layer, is_clear } => {
                let key = (obj, layer);
                if is_clear {
                    vfx_open.remove(&key);
                } else {
                    vfx_open.insert(key, line);
                }
            }
            LifecycleEvent::AudioOpen { channel } => {
                audio_open.entry(channel).or_insert(line);
            }
            LifecycleEvent::AudioClose { channel } => {
                audio_open.remove(&channel);
            }
        }
    }

    // One past the last line: sorts after every real boundary and reads as "past the end". It is
    // NOT used as an insertion index - EOF fixes can't follow the mid-script "bare line before the
    // boundary line" convention (there's no boundary call to land inside, and this index points
    // past the terminal `@<| is_end()/jump_to()/branch() |>`). apply_lifecycle_autofixes splices
    // EOF fixes into a fresh lazy block placed *before* that terminator instead. This value is
    // still the boundary_line stamped on EOF issues for reporting.
    let final_line = script.lines().count() as u32 + 1;
    flush_lifecycle_boundary(final_line, "eof", &mut tint_open, &mut env_tint_open, &mut vfx_open, &mut audio_open, &show_open, &mut issues);

    issues
}

/// Position slot for a character sprite, bucketed from the x component of show()'s `coord`.
/// Thresholds calibrated against the project's actual coordinate scale: test_pose.txt shows four
/// characters placed at x = -3/-1/1/3, and x = ±0.6 is explicitly described as "intentionally
/// overlapping". So the Left/Center/Right boundaries are at ±1.0, meaning anything inside
/// (-1.0, 1.0) is "Center". This catches cases like the model placing two characters at x=0.2
/// and x=0.4 (both Center → conflict) while correctly separating x=-2 and x=2 (Left vs Right).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum PositionSlot { Left, Center, Right }

fn x_to_slot(x: f32) -> PositionSlot {
    if x < -1.0 { PositionSlot::Left }
    else if x > 1.0 { PositionSlot::Right }
    else { PositionSlot::Center }
}

/// Extracts the x component from a coord array literal that starts at `rest`, e.g.
/// `[0.35, -0.3, 0.53, 0, 0]` → `Some(0.35)`. Returns `None` for `null` or anything that
/// isn't a numeric literal at the first position.
fn parse_coord_x(rest: &str) -> Option<f32> {
    let s = rest.trim_start();
    let inner = s.strip_prefix('[')?.trim_start();
    let x_str: String = inner.chars().take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-').collect();
    x_str.parse::<f32>().ok()
}

/// Skips past a quoted arg and the comma that follows it. Returns the slice after the comma,
/// trimmed. Returns None if no comma follows (i.e. there is no next argument).
fn skip_to_next_arg<'a>(after_quoted: &'a str) -> Option<&'a str> {
    let comma = after_quoted.find(',')?;
    Some(after_quoted[comma + 1..].trim_start())
}

/// Scene objects that are never character sprites: bg/fg take a full image path as the 2nd arg
/// (not a pose alias), and cam has no visual "body" to fade. Only objects outside this list and
/// with a coordinate that implies a position slot are subject to conflict detection.
const SCENE_OBJECTS: [&str; 3] = ["bg", "fg", "cam"];

/// Detects show() calls that place a new character sprite at a position slot (Left/Center/Right,
/// inferred from the x value of the coord argument) already occupied by a *different* character.
/// When this happens, inserts a quick fade-out + hide() before the new show() so the previous
/// occupant exits cleanly. The "fade" is `tint(prev, [1,1,1,0], 0.3)` immediately followed by
/// `hide(prev)` + a tint reset - the tween may be visually cut short (both calls are in the same
/// GDScript frame), but the state is always clean for the next appearance. For a fully animated
/// exit the model should add entry-chained wait() calls via a `#hnt:` requirement.
///
/// hide() calls clear the occupancy map, so a character that's explicitly hidden before another
/// arrives at the same slot doesn't trigger a spurious conflict.
pub fn apply_position_conflict_fixes(script: &str) -> String {
    // Collect show() and hide() events sorted by byte offset so we process them in document order.
    enum SlotEvent<'a> {
        Show { obj: &'a str, slot: PositionSlot, offset: usize },
        Hide { obj: &'a str },
    }
    let mut events: Vec<(usize, SlotEvent)> = Vec::new();

    for call_start in find_call_starts(script, "show") {
        let rest = &script[call_start..];
        let Some((obj, after_obj)) = extract_quoted_arg(rest) else { continue };
        if SCENE_OBJECTS.contains(&obj) { continue }
        // Skip the image-path/pose second arg.
        let Some(after_path_start) = skip_to_next_arg(after_obj) else { continue };
        let after_path = if after_path_start.starts_with('"') || after_path_start.starts_with('\'') {
            let Some((_, rest2)) = extract_quoted_arg(after_path_start) else { continue };
            rest2
        } else if after_path_start.starts_with("null") {
            &after_path_start[4..]
        } else {
            continue
        };
        // Third arg is coord.
        let Some(coord_start) = skip_to_next_arg(after_path) else { continue };
        if !coord_start.starts_with('[') { continue } // null coord → no position
        let Some(x) = parse_coord_x(coord_start) else { continue };
        events.push((call_start, SlotEvent::Show { obj, slot: x_to_slot(x), offset: call_start }));
    }

    for call_start in find_call_starts(script, "hide") {
        let rest = &script[call_start..];
        let Some((obj, _)) = extract_quoted_arg(rest) else { continue };
        events.push((call_start, SlotEvent::Hide { obj }));
    }

    events.sort_by_key(|(offset, _)| *offset);

    // Walk events in order, tracking which character occupies each slot.
    let mut slot_occupant: std::collections::HashMap<PositionSlot, String> = std::collections::HashMap::new();
    // (line number 1-indexed, text to insert before that line) - collected for bottom-to-top splice.
    let mut insertions: Vec<(usize, String)> = Vec::new();

    for (_, event) in &events {
        match event {
            SlotEvent::Show { obj, slot, offset } => {
                // Remove this obj from any slot it previously occupied (it's moving).
                slot_occupant.retain(|_, v| v != obj);
                if let Some(prev) = slot_occupant.get(slot) {
                    if prev != obj {
                        let prev_owned = prev.clone();
                        let line = byte_offset_to_line(script, *offset);
                        insertions.push((
                            line as usize,
                            format!(
                                "# 自动退场（{prev_owned} 原先占用该位置）\ntint(\"{prev_owned}\", [1,1,1,0], 0.3)\nhide(\"{prev_owned}\")\ntint(\"{prev_owned}\", [1,1,1,1], 0)"
                            ),
                        ));
                    }
                }
                slot_occupant.insert(*slot, obj.to_string());
            }
            SlotEvent::Hide { obj } => {
                slot_occupant.retain(|_, v| v != obj);
            }
        }
    }

    if insertions.is_empty() {
        return script.to_string();
    }

    // Apply insertions bottom-to-top so earlier ones don't shift the line numbers later ones
    // still reference. Within each insertion, lines are inserted sequentially (offset 0,1,2,...)
    // at consecutive positions so they land in the correct order.
    insertions.sort_by_key(|(line, _)| std::cmp::Reverse(*line));
    let mut lines: Vec<String> = script.lines().map(str::to_string).collect();
    for (line_num, text) in insertions {
        let insert_at = (line_num.saturating_sub(1)).min(lines.len());
        for (offset, insert_line) in text.lines().enumerate() {
            lines.insert(insert_at + offset, insert_line.to_string());
        }
    }
    lines.join("\n")
}

/// Splits `check_lifecycle_issues` findings into mechanical fixes (tint/env_tint/vfx/audio - one
/// unambiguous closed value, applied directly with no model round trip) and judgment calls (show -
/// returned for the caller to feed back to the model). Mid-script fixes are inserted as bare lines
/// immediately before their boundary call, bottom-to-top so earlier insertions don't shift the
/// line numbers later ones still need. EOF fixes can't follow that convention (there's no boundary
/// call to land inside, and a bare top-level GDScript call outside any `<| |>` block would parse
/// as dialogue text) - they're wrapped in a fresh lazy block placed before the final node's
/// terminator (`@<| is_end()/jump_to()/branch() |>`), which stays the last eager block.
pub fn apply_lifecycle_autofixes(script: &str) -> (String, Vec<LifecycleIssue>) {
    let issues = check_lifecycle_issues(script);

    let mut lines: Vec<String> = script.lines().map(str::to_string).collect();

    // Mid-script fixes first, bottom-to-top so earlier insertions don't shift the lines later ones
    // still target. The boundary here is a real `trans*` / `show`-switch / `# vvn:seq end` line
    // that itself lives inside a lazy block, so inserting the bare fix line immediately before it
    // lands the call inside that block - valid NovaScript.
    let mut mid_fixes: Vec<&LifecycleIssue> = issues
        .iter()
        .filter(|issue| issue.auto_fix.is_some() && issue.boundary_kind != "eof")
        .collect();
    mid_fixes.sort_by_key(|issue| std::cmp::Reverse(issue.boundary_line));
    for issue in mid_fixes {
        let Some(fix_line) = &issue.auto_fix else { continue };
        let insert_at = (issue.boundary_line.saturating_sub(1) as usize).min(lines.len());
        lines.insert(insert_at, fix_line.clone());
    }

    // EOF fixes need different handling: at end-of-script there's no boundary call to fall inside,
    // and the fixes are raw calls (`vfx(...)`, `stop(...)`, ...) that would be a parse error at top
    // level, so they get wrapped in one fresh lazy block. That block must go *before* the final
    // node's terminator - `@<| is_end() |>` (or jump_to/branch) is a terminal statement, nothing
    // valid follows it. The terminator is the last eager `@<|` block in the file (node labels are
    // the only other `@<|` lines, and every terminator sits after its node's label). Computed after
    // the mid-script pass so the position accounts for those insertions. Absent a terminator (an
    // EDIT-mode fragment), a trailing lazy block is valid on its own.
    let eof_fixes: Vec<String> = issues
        .iter()
        .filter(|issue| issue.boundary_kind == "eof")
        .filter_map(|issue| issue.auto_fix.clone())
        .collect();
    if !eof_fixes.is_empty() {
        let terminator_at = lines
            .iter()
            .rposition(|line| line.trim_start().starts_with("@<|"))
            .unwrap_or(lines.len());
        let mut block = vec!["<|".to_string()];
        block.extend(eof_fixes);
        block.push("|>".to_string());
        for (offset, fix_line) in block.into_iter().enumerate() {
            lines.insert(terminator_at + offset, fix_line);
        }
    }

    let remaining: Vec<LifecycleIssue> = issues.into_iter().filter(|issue| issue.auto_fix.is_none()).collect();
    (lines.join("\n"), remaining)
}

/// Formats the judgment-only (non-auto-fixable, i.e. `Show`) issues `apply_lifecycle_autofixes`
/// returns into a single message. These are advisory only - they never gate the retry loop (a
/// character still on screen is valid NovaScript that reload accepts; whether it *should* be is a
/// content call, left to the author in preview), so this message rides along on a retry triggered
/// by a real failure rather than causing one. EOF ("still on screen when the script ends") is
/// phrased separately from a mid-script scene switch: an ending tableau is usually intentional.
pub fn format_lifecycle_issues(issues: &[LifecycleIssue]) -> String {
    let details: Vec<String> = issues
        .iter()
        .map(|issue| {
            if issue.boundary_kind == "eof" {
                format!(
                    "第{}行对 \"{}\" 调用了 show，脚本结束时它仍在场且没有 hide。如果这是有意的结局留白（角色停在最后一幕）就无需处理；只有当你希望它在结尾前消失时，才补一条 hide(\"{}\")",
                    issue.opened_at_line, issue.object, issue.object
                )
            } else {
                format!(
                    "第{}行对 \"{}\" 调用了 show，但第{}行的场景切换（{}）之前没有 hide，请确认该对象是否应该在新场景里继续显示——需要的话补一条 hide(\"{}\")，如果确实要保留，也请在脚本里体现出这是有意保留的",
                    issue.opened_at_line, issue.object, issue.boundary_line, issue.boundary_kind, issue.object
                )
            }
        })
        .collect();
    format!("脚本中存在跨场景未关闭的显示状态：{}", details.join("；"))
}

/// A `# vvn:seq begin` marker with no matching `# vvn:seq end` before EOF. This both means the
/// rest of the file silently loses scene-boundary cleanup checking (since `check_lifecycle_issues`
/// stays in suppressed mode once `in_sequence` is set and only the end marker clears it) and
/// usually signals a simple authoring mistake - the performance-sequence span was never closed.
#[derive(Debug, Clone, PartialEq)]
pub struct UnclosedSequenceIssue {
    pub opened_at_line: u32,
}

/// Pairs up begin/end sequence markers in document order (a plain stack: push on begin, pop on
/// end) and reports any begin left on the stack at EOF. Operates on the final generated script
/// text, independent of how many `#seq:` spans the original dialogue-only input declared.
pub(crate) fn check_unclosed_sequences(script: &str) -> Vec<UnclosedSequenceIssue> {
    let mut markers: Vec<(usize, bool)> = Vec::new();
    for offset in find_marker_lines(script, world_state::SEQUENCE_BEGIN_MARKER) {
        markers.push((offset, true));
    }
    for offset in find_marker_lines(script, world_state::SEQUENCE_END_MARKER) {
        markers.push((offset, false));
    }
    markers.sort_by_key(|(offset, _)| *offset);

    let mut open_stack: Vec<u32> = Vec::new();
    for (offset, is_start) in markers {
        let line = byte_offset_to_line(script, offset);
        if is_start {
            open_stack.push(line);
        } else {
            open_stack.pop();
        }
    }

    open_stack.into_iter().map(|opened_at_line| UnclosedSequenceIssue { opened_at_line }).collect()
}

pub fn format_unclosed_sequence_issues(issues: &[UnclosedSequenceIssue]) -> String {
    let details: Vec<String> = issues
        .iter()
        .map(|issue| format!("第{}行开始的演出序列（# vvn:seq begin）没有对应的 # vvn:seq end，请补上收尾标记", issue.opened_at_line))
        .collect();
    format!("脚本中存在未闭合的演出序列：{}", details.join("；"))
}

const TERMINAL_CALL_KINDS: [&str; 4] = ["label", "is_end", "jump_to", "branch"];

/// Whether the script's last node was ever explicitly closed - see novascript-reference.md §1.3's
/// hard constraint: only the *next* eager block (the next `label()`, or
/// `is_end()`/`jump_to()`/`branch()`) flushes a node's trailing dialogue into a `DialogueEntry`.
/// The engine's end-of-file auto `is_end()` insertion only resets node-type bookkeeping, not this
/// flush, so a last node with no explicit closer silently loses its trailing dialogue.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalClosureIssue {
    pub node_label: Option<String>,
    pub label_line: u32,
    pub last_content_line: u32,
}

fn last_non_blank_line(script: &str) -> u32 {
    let lines: Vec<&str> = script.lines().collect();
    lines
        .iter()
        .enumerate()
        .rev()
        .find(|(_, line)| !line.trim().is_empty())
        .map(|(index, _)| index as u32 + 1)
        .unwrap_or(1)
}

/// Only the very last of `label`/`is_end`/`jump_to`/`branch` (by position) matters: an earlier
/// node missing an explicit closer is fine, since the *next* node's `label()` call is itself the
/// eager block that flushes its predecessor's trailing dialogue - see §1.3's exact wording. A
/// script with none of these four calls at all is a single implicit node that still needs an
/// explicit closer.
pub(crate) fn check_terminal_closure(script: &str) -> Vec<TerminalClosureIssue> {
    let mut occurrences: Vec<(usize, &str)> = Vec::new();
    for call_name in TERMINAL_CALL_KINDS {
        for call_start in find_call_starts(script, call_name) {
            occurrences.push((call_start, call_name));
        }
    }
    occurrences.sort_by_key(|(offset, _)| *offset);

    let last_content_line = last_non_blank_line(script);
    match occurrences.last() {
        None => vec![TerminalClosureIssue { node_label: None, label_line: 1, last_content_line }],
        Some((offset, kind)) if *kind == "label" => {
            let node_label = extract_quoted_arg(&script[*offset..]).map(|(name, _)| name.to_string());
            vec![TerminalClosureIssue { node_label, label_line: byte_offset_to_line(script, *offset), last_content_line }]
        }
        Some(_) => Vec::new(),
    }
}

/// Matches the other `format_*_issues` functions' tone: explain the failure mode in plain
/// language and suggest the exact fix (per the prompt's request, an `@<| is_end() |>` line) rather
/// than silently inserting it - unlike the lifecycle tracker's mechanical fixes, closure structure
/// is cheap enough for the model to get right once told exactly where it's missing.
pub fn format_terminal_closure_issues(issues: &[TerminalClosureIssue]) -> String {
    issues
        .iter()
        .map(|issue| {
            let label_desc = issue.node_label.as_ref().map(|name| format!("（\"{name}\"）")).unwrap_or_default();
            format!(
                "脚本最后一个节点{label_desc}在文件末尾之前没有用 is_end()/jump_to()/branch() 收尾——根据 NovaScript 解析规则，最后一段对话/lazy 块会被静默丢弃，该节点的对话数会变成 0。请在第{}行之后补一行 @<| is_end() |>（如果这是一个有名字的结局，用 @<| is_end(\"结局名\") |>；如果应该跳转或分支，改用对应的 jump_to()/branch() 收尾）。",
                issue.last_content_line
            )
        })
        .collect::<Vec<_>>()
        .join("；")
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
        "You are editing or generating a NovaScript script for Nova2 that will be saved as a .txt file.\n        Treat the NovaScript reference, animation/VFX notes, known-resource lists, and the current file content below as the sole ground truth.\n        Output must satisfy all of the following rules:\n        1. By default output only the script text itself. No explanations, titles, Markdown code fences, or wrappers.\n        2. If the script needs to declare new character nodes, you may output a single `#NEWCHARS: [{\"node\":\"...\",\"bind\":\"...\",\"folder\":\"...\"}]` line as the very first line of your response. No other metadata is allowed.\n        3. If `#NEWCHARS:` appears it must be the entire first line followed immediately by a valid JSON array.\n        4. After `#NEWCHARS:` the complete script starts on line 2.\n        5. The final script must be directly reloadable by the engine without errors.\n        6. If the Current File Content section below is non-empty, treat it as the sole ground truth: only modify what the user request explicitly asks for. Preserve everything else verbatim (dialogue, function calls, character names, resource paths, label names). Never rewrite wholesale or invent new content for sections not requested.\n        7. Only use character bind names from the Known Character Bind Names list, or names already present in the current content or reference docs. Do not invent plausible-sounding names outside the list. Use `#NEWCHARS` only for genuinely new characters.\n        8. Only generate content from scratch when the Current File Content section is empty (the file does not exist or is intentionally new).\n        9. By default do not modify dialogue text (what characters say, narration). Do not add, delete, merge, or reorder dialogue lines unless the user request explicitly names specific dialogue to change or uses phrasing like \"change the line\" or \"rewrite dialogue\". Non-dialogue elements (background/lighting/vfx/anim/transition/camera/music function calls and parameters) may be changed freely.\n        10. When the user request describes atmosphere, mood, weather, lighting, or pacing, implement it through vfx/anim/transition/lighting/ambient function adjustments - never by rewriting dialogue. If a specific dialogue line is truly needed (rare), confirm the user explicitly requested it; otherwise keep dialogue unchanged.\n        11. tint/env_tint/vfx/move etc. are persistent (state does not expire automatically). If this edit adds or modifies such effects, check whether any scene boundary exists after the edit point in the current content (signal: subsequent trans_fade/trans_left/trans_right/trans_up/trans_down, or another show(\"bg\", different_path)/show(\"fg\", different_path) without a hide/restore in between). If a boundary exists, add a restore call before it to prevent effect leakage into the next unrelated scene. This also applies within a scene: if an effect was gradually deepened to reflect escalating tension, roll it back at the narrative turning point - effects should track narrative tension, not just the file start/end.\n        12. Only reference paths that actually exist. Paths not in the Known Background Asset Paths list are forbidden even as plausible guesses. Non-character objects (bg/fg) show/trans* image paths must come from that list or from paths already in the current content. Character objects (bind names like ergong/gaotian) show() second argument must be a pose alias from Known Character Pose Aliases (e.g. normal/cry) - never a standings/ path, which is an internal engine composite-sprite path.\n        13. Likewise: play/fade_in track names from Known Audio Tracks; sound() names from Known Sound Effects; vfx() shader names from Known VFX Shaders. Never invent names from intuition or convention. If no suitable resource exists, skip the detail or use an existing resource rather than fabricating a name.\n        14. The entry parameter is NovaScript's only sequencing mechanism: statements in a `<| ... |>` block execute immediately in GDScript order - they do not wait for each other. wait()/vfx()/move() etc. return a chain tail; only passing that value to the next call's entry schedules it after the wait, otherwise everything fires simultaneously from the animation root. To chain A then pause then B on the same object: `var e = vfx(\"cam\", \"glitch\", 0.9, 0.05)\ne = wait(0.05, e)\ne = vfx(\"cam\", \"glitch\", 0, 0.06, null, e)`. Also vfx(\"cam\", shader_layer, ...) defaults to layer 0; use explicit layer_ids (0-3) to stack multiple screen effects without overwriting each other.\n        15. Coordinate array format for show(): must be exactly 5 elements [x, y, scale, rx, ry]. Missing any element causes out-of-bounds engine failure and the character will not appear. y is typically -0.3, rx/ry typically 0. Example: show(bind_name, pose_alias, [-2, -0.3, 0.53, 0, 0]). Typical layouts: single center [0,-0.3,0.53,0,0]; two at [-2,-0.3,0.53,0,0] and [2,-0.3,0.53,0,0]; three at ~-2.5/0/2.5; four at ~-3/-1/1/3 scale~0.4. Enter from off-screen with x=4 or x=-4 then move(). x=0.6 is deliberate overlap territory - avoid |x|<1 for multi-character layouts. Keep scale changes within 30-40% of baseline unless a close-up is explicitly requested.\n        16. Never use \"cam\" as the obj for tint/env_tint: \"cam\" is Camera3D with no modulate property - tint(\"cam\",...) throws NullReferenceException and crashes the tween, it is not a silent failure. tint/env_tint only work on bg/fg/character objects. For whole-screen color overlay use vfx(\"cam\", [\"color\", layer_id], t, duration, {\"_ColorMul\": ...}).".to_string(),
        format!("## NovaScript 参考文档\n\n{reference_text}"),
    ];

    let known_characters = list_known_character_bind_names(nova2_project_dir);
    if !known_characters.is_empty() {
        sections.push(format!("## 已知角色绑定名\n\n{}", known_characters.join(", ")));
    }

    let character_poses = list_known_character_poses(nova2_project_dir);
    if !character_poses.is_empty() {
        let pose_lines: Vec<String> = character_poses
            .iter()
            .map(|(char_name, poses)| format!("- {char_name}: {}", poses.join(", ")))
            .collect();
        sections.push(format!(
            "## 已知角色 pose 别名（show(角色绑定名, pose别名) 的第二个参数只能用这些别名，不能用 standings/ 路径）\n\n{}",
            pose_lines.join("\n")
        ));
    }

    let known_asset_paths = list_known_asset_script_paths(nova2_project_dir);
    if !known_asset_paths.is_empty() {
        sections.push(format!("## 已知背景资源路径\n\n{}", known_asset_paths.join(", ")));
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
        "## Current Content of Target File {target_file}\n\n{}",
        if existing_content.is_empty() {
            "(empty - this file does not exist yet)".to_string()
        } else {
            existing_content.to_string()
        }
    ));

    sections.push(format!("## 用户需求\n\n{user_prompt}"));
    Ok(sections.join("\n\n"))
}

/// Per node: groups consecutive moments sharing the same `background` into one descriptive line
/// each (the natural generalization of the old one-line-per-scene format, computed post-hoc from
/// each moment's sticky `#loc:` annotation rather than relying on any pre-grouped scene list,
/// since `#loc:` no longer drives node structure - see Correction 1), flags `RawPassthrough` spans
/// so the model knows not to touch them, and surfaces `Branch`/`Jump` terminators' destinations
/// (and, for `Branch`, each option's text/mode/cond) so the model understands the node's exit
/// point exists without being allowed to modify it.
fn format_world_state_timeline(timeline: &world_state::WorldStateTimeline) -> String {
    let mut lines = Vec::new();
    if !timeline.distinct_speakers.is_empty() {
        lines.push(format!("- 本次对话涉及的说话人（显示名，按首次出现顺序）：{}", timeline.distinct_speakers.join("、")));
    }

    for node in &timeline.nodes {
        let node_label = node.name.as_deref().unwrap_or("主节点");
        lines.push(format!("- 节点 \"{node_label}\"："));

        let mut index = 0;
        let mut beat_number = 0;
        while index < node.items.len() {
            match &node.items[index] {
                world_state::NodeItem::RawPassthrough(_) => {
                    lines.push("  - （这里有一段作者手写的演出内容，已原样保留在骨架中，不需要你处理）".to_string());
                    index += 1;
                }
                world_state::NodeItem::Moment(first) => {
                    let background = first.background.as_deref();
                    let mut group_end = index + 1;
                    while group_end < node.items.len() {
                        match &node.items[group_end] {
                            world_state::NodeItem::Moment(moment) if moment.background.as_deref() == background => group_end += 1,
                            _ => break,
                        }
                    }
                    beat_number += 1;
                    let on_stage = node.items[index..group_end]
                        .iter()
                        .filter_map(|item| match item {
                            world_state::NodeItem::Moment(moment) => Some(moment),
                            world_state::NodeItem::RawPassthrough(_) => None,
                        })
                        .last()
                        .map(|moment| moment.on_stage.join("、"))
                        .unwrap_or_default();
                    let background_desc = background.unwrap_or("（未标注背景，沿用之前的画面，或视为不需要切换背景）");
                    lines.push(format!(
                        "  - 第 {beat_number} 段：背景 {background_desc}；在场角色（按出现顺序）：{}",
                        if on_stage.is_empty() { "（无对话角色，可能是纯旁白）".to_string() } else { on_stage }
                    ));
                    index = group_end;
                }
            }
        }

        match &node.terminator {
            world_state::NodeTerminator::End => {}
            world_state::NodeTerminator::Jump(dest) => {
                lines.push(format!("  - 本节点结束后跳转到节点 \"{dest}\"（骨架里已经写好 jump_to，不能改动）"));
            }
            world_state::NodeTerminator::Branch(options) => {
                let option_descs: Vec<String> = options
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
                lines.push(format!("  - 本节点以分支结束（骨架里已经写好 branch，不能改动），选项：{}", option_descs.join("；")));
            }
        }
    }

    lines.join("\n")
}

/// AUTOSTAGE's prompt builder - deliberately separate from `build_generation_prompt` rather than
/// sharing it, since the two modes' rule sets actively conflict (edit mode forbids touching
/// existing content/dialogue by default; AUTOSTAGE's whole job is generating new content and
/// dialogue is given, not editable). Reuses the same known-resource list helpers and the full
/// reference doc; the model's freedom is bounded by the world-state timeline and the pre-built
/// node skeleton rather than by "don't touch existing content" rules, since there is no existing
/// content here. Rule 11 (persistent-effect reversion before scene boundaries) from
/// `build_generation_prompt` is deliberately NOT repeated here as a prose instruction - it's
/// exactly the rule that's proven not to work, and AUTOSTAGE leans on the lifecycle tracker
/// (`check_lifecycle_issues`/`apply_lifecycle_autofixes`) as the real enforcement instead.
pub fn build_autostage_prompt(
    nova2_project_dir: &Path,
    vfx_notes_path: &str,
    target_file: &str,
    timeline: &world_state::WorldStateTimeline,
    skeleton: &str,
) -> Result<String, BackendError> {
    let reference_path = nova2_project_dir.join("novascript-reference.md");
    let reference_text = fs::read_to_string(&reference_path).map_err(|error| {
        BackendError::message(format!("读取 {} 失败: {error}", reference_path.display()))
    })?;

    let mut sections = vec![
        "You are filling in a pre-structured NovaScript skeleton for Nova2 - the dialogue is already in place; your job is to add the staging.\n        Treat the NovaScript reference, known-resource lists, world state timeline, and node skeleton below as the sole ground truth.\n        Output must satisfy all of the following rules:\n        1. Output only the script text itself. No explanations, titles, Markdown code fences, or wrappers.\n        2. If any speaker in the dialogue is not in the Known Character Bind Names list, you may output a single `#NEWCHARS: [{\"node\":\"...\",\"bind\":\"...\",\"folder\":\"...\"}]` line as the very first line. No other metadata is allowed.\n        3. If `#NEWCHARS:` appears it must be the entire first line followed immediately by a valid JSON array.\n        4. After `#NEWCHARS:` the complete script starts on line 2.\n        5. The final script must be directly reloadable by the engine without errors.\n        6. The Node Skeleton section below has already established all structural eager calls (label/jump_to/is_end/branch) and node boundaries - preserve them verbatim. Your only job is to replace or expand the placeholder comments inside each `<| ... |>` block with real staging function calls. Do not modify, delete, or move any structural calls or node splits, and do not add or remove nodes. If a placeholder block contains a Staging Requirement line in addition to the TODO comment, that is a concrete technical requirement written by the author (e.g. specifying music or animation) - fulfill it with the corresponding function call; do not ignore it or treat it as a loose suggestion. If a placeholder block has been replaced with content wrapped in `# vvn:seq begin` / `# vvn:seq end`, that is complete author-written staging code - preserve it character-for-character; do not add, remove, or alter anything between those two lines.\n        7. Every dialogue line immediately following a placeholder block in the skeleton must be preserved verbatim. Do not rewrite, add, delete, merge, or reorder them. This task adds staging around dialogue, it does not rewrite dialogue.\n        8. Only apply show/tint/move operations to character objects listed as on-stage in the World State Timeline for that beat. Do not introduce characters absent from the timeline. Background switches are determined by the background annotation on each timeline beat - place the corresponding show call in the correct placeholder block; do not decide independently whether to switch backgrounds or split nodes. Node structure (label/jump_to/branch count and layout) is already fixed by the skeleton.\n        9. Only reference paths that actually exist. Paths not in the Known Background Asset Paths list are forbidden even as plausible guesses (e.g. do not shorten backgrounds/room to room). Non-character objects (bg/fg) show/trans* image paths must come from that list; world-state-annotated background paths must be used verbatim. Character objects (bind names like ergong/gaotian) show() second argument must be a pose alias from Known Character Pose Aliases (e.g. normal/cry) - never a standings/ path, which is an internal engine composite-sprite path, not a NovaScript API parameter.\n        10. Likewise: play/fade_in track names from Known Audio Tracks; sound() names from Known Sound Effects; vfx() shader names from Known VFX Shaders. Never invent names from intuition. If no suitable resource exists, skip the detail or use an existing resource.\n        11. The entry parameter is NovaScript's only sequencing mechanism: statements in a `<| ... |>` block execute immediately in GDScript order - they do not wait for each other. wait()/vfx()/move() etc. return a chain tail; only passing that value to the next call's entry schedules it after the wait. To chain A then pause then B: `var e = vfx(\"cam\", \"glitch\", 0.9, 0.05)\ne = wait(0.05, e)\ne = vfx(\"cam\", \"glitch\", 0, 0.06, null, e)`. vfx(\"cam\", shader_layer, ...) defaults to layer 0; use explicit layer_ids (0-3) to stack multiple effects.\n        12. Coordinate array format for show(): must be exactly 5 elements [x, y, scale, rx, ry]. Missing any element causes out-of-bounds engine failure and the character will not appear. y is typically -0.3, rx/ry typically 0. Example: show(bind_name, pose_alias, [-2, -0.3, 0.53, 0, 0]). Typical layouts: single center [0,-0.3,0.53,0,0]; two at [-2,-0.3,0.53,0,0] and [2,-0.3,0.53,0,0]; three at ~-2.5/0/2.5; four at ~-3/-1/1/3 scale~0.4. Enter from off-screen with x=4 or x=-4 then move(). x=0.6 is deliberate overlap territory - avoid |x|<1 for multi-character layouts. Keep scale changes within 30-40% of baseline.\n        13. Never use \"cam\" as the obj for tint/env_tint: \"cam\" is Camera3D with no modulate property - tint(\"cam\",...) throws an exception and crashes the tween. tint/env_tint only work on bg/fg/character objects. For whole-screen color overlay use vfx(\"cam\", [\"color\", layer_id], t, duration, {\"_ColorMul\": ...}).\n        14. You do not need to worry about cleaning up persistent effects (tint/env_tint/vfx/play) across scene boundaries - that is handled deterministically by post-generation code that auto-patches or reports issues separately. Focus on the staging each individual beat needs; do not add unnecessary cleanup calls at scene ends.".to_string(),
        format!("## NovaScript 参考文档\n\n{reference_text}"),
    ];

    let known_characters = list_known_character_bind_names(nova2_project_dir);
    if !known_characters.is_empty() {
        sections.push(format!("## 已知角色绑定名\n\n{}", known_characters.join(", ")));
    }

    let character_poses = list_known_character_poses(nova2_project_dir);
    if !character_poses.is_empty() {
        let pose_lines: Vec<String> = character_poses
            .iter()
            .map(|(char_name, poses)| format!("- {char_name}: {}", poses.join(", ")))
            .collect();
        sections.push(format!(
            "## 已知角色 pose 别名（show(角色绑定名, pose别名) 的第二个参数只能用这些别名，不能用 standings/ 路径）\n\n{}",
            pose_lines.join("\n")
        ));
    }

    let known_asset_paths = list_known_asset_script_paths(nova2_project_dir);
    if !known_asset_paths.is_empty() {
        sections.push(format!("## 已知背景资源路径\n\n{}", known_asset_paths.join(", ")));
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

    let vfx_notes_path = vfx_notes_path.trim();
    if !vfx_notes_path.is_empty() {
        let path = Path::new(vfx_notes_path);
        if path.exists() {
            let vfx_notes = fs::read_to_string(path)
                .map_err(|error| BackendError::message(format!("读取 {} 失败: {error}", path.display())))?;
            sections.push(format!("## 动画/VFX 说明\n\n{vfx_notes}"));
        }
    }

    sections.push(format!("## 世界状态时间线\n\n{}", format_world_state_timeline(timeline)));
    sections.push(format!(
        "## 目标文件 {target_file} 的节点骨架（必须逐字保留结构和台词，只能在每个 `<| ... |>` 占位块内部把占位注释替换成真正的演出调用）\n\n{skeleton}"
    ));

    Ok(sections.join("\n\n"))
}

#[cfg(test)]
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
            None => format!("写了 \"{}\", 但项目里没有这个资源，请改用已知背景资源路径列表中的真实路径", issue.written_path),
        })
        .collect();
    format!("Script references non-existent asset paths: {}", details.join("；"))
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
        assert!(prompt.contains("Known Character Bind Names"));
        assert!(prompt.contains("ergong"));
        assert!(prompt.contains("verbatim"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prompt_marks_target_as_new_file_when_content_is_empty() {
        let dir = make_fixture_project(None);
        let prompt = build_generation_prompt(&dir, "", "new_chapter.txt", "", "写一个新故事", None).unwrap();

        assert!(prompt.contains("does not exist yet"));

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

        assert!(prompt.contains("do not modify dialogue text"));
        assert!(prompt.contains("never by rewriting dialogue"));

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

    #[test]
    fn autostage_prompt_surfaces_hint_requirement_and_instructs_model_to_honor_it() {
        let dir = make_fixture_project(None);
        let dialogue = "#hnt: 配雨声环境音\nergong::外面下雨了呢\n";
        let timeline = world_state::derive_world_state_timeline(dialogue, "ch1_autostage").unwrap();
        let skeleton = world_state::build_node_skeleton(&timeline, "ch1_autostage");
        let prompt = build_autostage_prompt(&dir, "", "ch1.txt", &timeline, &skeleton).unwrap();

        assert!(prompt.contains("Staging Requirement"));
        assert!(prompt.contains("配雨声环境音"));
        assert!(prompt.contains("fulfill it with the corresponding function call"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn autostage_prompt_includes_skeleton_and_world_state_timeline() {
        let dir = make_fixture_project(None);
        let dialogue = "#loc: backgrounds/room\nergong::你好\n张浅野::早上好\n";
        let timeline = world_state::derive_world_state_timeline(dialogue, "ch1_autostage").unwrap();
        let skeleton = world_state::build_node_skeleton(&timeline, "ch1_autostage");
        let prompt = build_autostage_prompt(&dir, "", "ch1.txt", &timeline, &skeleton).unwrap();

        assert!(prompt.contains("世界状态时间线"));
        assert!(prompt.contains("ergong"));
        assert!(prompt.contains("张浅野"));
        assert!(prompt.contains("backgrounds/room"));
        assert!(prompt.contains("label(\"ch1_autostage\""));
        assert!(prompt.contains("is_end()"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn autostage_prompt_omits_edit_mode_preserve_unchanged_rule() {
        let dir = make_fixture_project(None);
        let dialogue = "ergong::你好\n";
        let timeline = world_state::derive_world_state_timeline(dialogue, "ch1_autostage").unwrap();
        let skeleton = world_state::build_node_skeleton(&timeline, "ch1_autostage");
        let prompt = build_autostage_prompt(&dir, "", "ch1.txt", &timeline, &skeleton).unwrap();

        // Rule 6 in build_generation_prompt is specific to editing existing content; AUTOSTAGE
        // generates fresh staging around given dialogue, so that wording must not leak in here.
        assert!(!prompt.contains("only modify what the user request explicitly asks for"));
        // The dialogue-preservation rule still applies, just phrased for AUTOSTAGE's own task.
        assert!(prompt.contains("preserved verbatim"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn skeleton_alone_passes_terminal_closure_and_lifecycle_checks() {
        // The skeleton has no show/tint/vfx/play calls yet (the model fills those in), and always
        // ends with is_end() by construction - so it must satisfy Phase 1/2's validators with zero
        // issues even before any staging is added. This is what lets AUTOSTAGE skip the model
        // remembering closure/lifecycle rules entirely for the structural parts.
        let dialogue = "#loc: backgrounds/room\nergong::你好\n张浅野::早上好\n\n#loc: backgrounds/corridor\nergong::走吧\n";
        let timeline = world_state::derive_world_state_timeline(dialogue, "ch1_autostage").unwrap();
        let skeleton = world_state::build_node_skeleton(&timeline, "ch1_autostage");

        assert!(check_terminal_closure(&skeleton).is_empty());
        assert!(check_lifecycle_issues(&skeleton).is_empty());
    }

    #[test]
    fn autostage_prompt_includes_known_character_and_asset_lists() {
        let dir = make_fixture_project(None);
        write_background_fixture(&dir, "room");
        let dialogue = "ergong::你好\n";
        let timeline = world_state::derive_world_state_timeline(dialogue, "ch1_autostage").unwrap();
        let skeleton = world_state::build_node_skeleton(&timeline, "ch1_autostage");
        let prompt = build_autostage_prompt(&dir, "", "ch1.txt", &timeline, &skeleton).unwrap();

        assert!(prompt.contains("Known Character Bind Names"));
        assert!(prompt.contains("Known Background Asset Paths"));
        assert!(prompt.contains("backgrounds/room"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn autostage_prompt_includes_amended_skeleton_preservation_rule_mentioning_seq_markers() {
        let dir = make_fixture_project(None);
        let dialogue = "ergong::你好\n";
        let timeline = world_state::derive_world_state_timeline(dialogue, "ch1_autostage").unwrap();
        let skeleton = world_state::build_node_skeleton(&timeline, "ch1_autostage");
        let prompt = build_autostage_prompt(&dir, "", "ch1.txt", &timeline, &skeleton).unwrap();

        assert!(prompt.contains("vvn:seq begin"));
        assert!(prompt.contains("complete author-written staging code"));

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
        assert!(prompt.contains("layer_id"));
        assert!(prompt.contains("baseline"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prompt_extends_scope_leakage_rule_to_narrative_easing() {
        let dir = make_fixture_project(Some("ergong::你好\n"));
        let prompt = build_generation_prompt(&dir, "", "ch1.txt", "ergong::你好\n", "对峙更紧张", None).unwrap();

        assert!(prompt.contains("turning point"));
        assert!(prompt.contains("track narrative tension"));

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

    #[test]
    fn accepts_show_then_hide_before_boundary() {
        let script = "show(\"yuki\", \"normal\")\nhide(\"yuki\")\ntrans_fade(\"cam\", \"backgrounds/toilet\", 2)\n";
        let issues = check_lifecycle_issues(script);
        assert!(issues.iter().all(|issue| issue.object != "yuki"));
    }

    #[test]
    fn flags_show_left_open_across_trans_boundary() {
        let script = "show(\"yuki\", \"normal\")\ntrans_fade(\"cam\", \"backgrounds/toilet\", 2)\n";
        let issues = check_lifecycle_issues(script);
        // "yuki" is never hidden, so it's re-flagged at every boundary it's still open at - here
        // both the trans_fade boundary and the implicit EOF boundary right after it.
        let show_issues: Vec<_> = issues.iter().filter(|issue| issue.call_kind == LifecycleCallKind::Show).collect();
        assert_eq!(show_issues.len(), 2);
        let trans_issue = show_issues.iter().find(|issue| issue.boundary_kind == "trans_fade").unwrap();
        assert_eq!(trans_issue.object, "yuki");
        assert_eq!(trans_issue.opened_at_line, 1);
        assert_eq!(trans_issue.boundary_line, 2);
        assert!(trans_issue.auto_fix.is_none());
    }

    #[test]
    fn accepts_tint_reverted_to_default_before_boundary() {
        let script = "tint(\"bg\", [0.55,0.6,0.68], 0)\ntint(\"bg\", [1,1,1,1], 0)\ntrans_fade(\"cam\", \"backgrounds/toilet\", 2)\n";
        let issues = check_lifecycle_issues(script);
        assert!(issues.iter().all(|issue| issue.call_kind != LifecycleCallKind::Tint));
    }

    #[test]
    fn auto_fixes_tint_left_open_before_boundary() {
        let script = "tint(\"bg\", [0.55,0.6,0.68], 0)\ntrans_fade(\"cam\", \"backgrounds/toilet\", 2)\n";
        let (patched, remaining) = apply_lifecycle_autofixes(script);
        assert!(remaining.is_empty());
        let lines: Vec<&str> = patched.lines().collect();
        let trans_index = lines.iter().position(|line| line.starts_with("trans_fade")).unwrap();
        assert_eq!(lines[trans_index - 1], "tint(\"bg\", [1,1,1,1], 0)");
    }

    #[test]
    fn accepts_play_stopped_before_boundary() {
        let script = "play(\"bgm\", \"prelude\", 0.5)\nstop(\"bgm\", 0)\ntrans_fade(\"cam\", \"backgrounds/toilet\", 2)\n";
        let issues = check_lifecycle_issues(script);
        assert!(issues.iter().all(|issue| issue.call_kind != LifecycleCallKind::Audio));
    }

    #[test]
    fn auto_fixes_play_left_running_across_boundary() {
        let script = "play(\"bgm\", \"prelude\", 0.5)\ntrans_fade(\"cam\", \"backgrounds/toilet\", 2)\n";
        let (patched, remaining) = apply_lifecycle_autofixes(script);
        assert!(remaining.is_empty());
        let lines: Vec<&str> = patched.lines().collect();
        let trans_index = lines.iter().position(|line| line.starts_with("trans_fade")).unwrap();
        assert_eq!(lines[trans_index - 1], "stop(\"bgm\", 0)");
    }

    #[test]
    fn eof_autofix_without_terminator_appends_a_wrapped_lazy_block() {
        // No `@<| |>` terminator (an EDIT-mode fragment): the audio cleanup must still be wrapped
        // in a lazy block rather than left as a bare top-level call (which would be a parse error).
        let script = "show(\"yuki\", \"normal\")\nplay(\"bgm\", \"prelude\", 0.5)\n";
        let (patched, _remaining) = apply_lifecycle_autofixes(script);
        let lines: Vec<&str> = patched.lines().collect();
        assert_eq!(lines[0], "show(\"yuki\", \"normal\")");
        assert_eq!(lines[1], "play(\"bgm\", \"prelude\", 0.5)");
        assert_eq!(lines[2], "<|");
        assert_eq!(lines[3], "stop(\"bgm\", 0)");
        assert_eq!(lines[4], "|>");
    }

    #[test]
    fn eof_autofix_wraps_in_lazy_block_before_terminal_is_end() {
        // The original bug: a cam-vfx cleanup was appended *after* `@<| is_end() |>` as a bare
        // top-level line, which the engine parsed as dialogue -> reload parse error. The fix must
        // land inside a fresh lazy block, immediately before the terminator, which stays last.
        let script = "<|\nvfx(\"cam\", \"shake\", 0.5, 0.1)\n|>\n旁白：雨还在下。\n@<| is_end() |>\n";
        let (patched, _remaining) = apply_lifecycle_autofixes(script);
        let lines: Vec<&str> = patched.lines().collect();
        assert_eq!(lines.last().copied(), Some("@<| is_end() |>"));
        let end_idx = lines.iter().position(|line| line.trim() == "@<| is_end() |>").unwrap();
        assert_eq!(lines[end_idx - 3], "<|");
        assert_eq!(lines[end_idx - 2], "vfx(\"cam\", [null, 0])");
        assert_eq!(lines[end_idx - 1], "|>");
        // The fix actually closed the layer: re-checking finds no vfx left open.
        assert!(check_lifecycle_issues(&patched).iter().all(|issue| issue.call_kind != LifecycleCallKind::Vfx));
    }

    #[test]
    fn does_not_flag_vfx_same_layer_overwrite_as_leak() {
        let script = "vfx(\"cam\", \"glitch\", 0.9, 0.05)\nvfx(\"cam\", \"shake\", 0.5, 0.1)\n";
        let issues = check_lifecycle_issues(script);
        let vfx_issues: Vec<_> = issues.iter().filter(|issue| issue.call_kind == LifecycleCallKind::Vfx).collect();
        assert_eq!(vfx_issues.len(), 1);
        assert_eq!(vfx_issues[0].object, "cam:0");
    }

    #[test]
    fn tracks_cam_vfx_layers_independently() {
        let script = "vfx(\"cam\", [\"color\", 0], 1, 0.5)\nvfx(\"cam\", [\"rain\", 1], 1, 0.5)\nvfx(\"cam\", [null, 1], 0, 0)\ntrans_fade(\"cam\", \"backgrounds/toilet\", 2)\n";
        let issues = check_lifecycle_issues(script);
        let vfx_issues: Vec<_> = issues.iter().filter(|issue| issue.call_kind == LifecycleCallKind::Vfx).collect();
        assert_eq!(vfx_issues.len(), 1);
        assert_eq!(vfx_issues[0].object, "cam:0");
    }

    #[test]
    fn entry_chaining_does_not_confuse_vfx_parsing() {
        let script = "var e = vfx(\"cam\", \"glitch\", 0.9, 0.05)\ne = wait(0.05, e)\ne = vfx(\"cam\", \"glitch\", 0, 0.06, null, e)\n";
        let issues = check_lifecycle_issues(script);
        let vfx_issues: Vec<_> = issues.iter().filter(|issue| issue.call_kind == LifecycleCallKind::Vfx).collect();
        assert_eq!(vfx_issues.len(), 1);
        assert_eq!(vfx_issues[0].object, "cam:0");
        assert_eq!(vfx_issues[0].boundary_kind, "eof");
    }

    #[test]
    fn repeated_show_bg_different_path_without_hide_is_a_boundary() {
        let script = "tint(\"yuki\", [0.3,0.3,0.3], 0)\nshow(\"bg\", \"backgrounds/room\")\nshow(\"bg\", \"backgrounds/toilet\")\n";
        let issues = check_lifecycle_issues(script);
        let tint_issues: Vec<_> = issues.iter().filter(|issue| issue.call_kind == LifecycleCallKind::Tint).collect();
        assert_eq!(tint_issues.len(), 1);
        assert_eq!(tint_issues[0].boundary_kind, "show_switch");
    }

    #[test]
    fn repeated_show_bg_same_path_is_not_a_boundary() {
        let script = "tint(\"yuki\", [0.3,0.3,0.3], 0)\nshow(\"bg\", \"backgrounds/room\")\nshow(\"bg\", \"backgrounds/room\")\n";
        let issues = check_lifecycle_issues(script);
        let tint_issues: Vec<_> = issues.iter().filter(|issue| issue.call_kind == LifecycleCallKind::Tint).collect();
        assert_eq!(tint_issues.len(), 1);
        assert_eq!(tint_issues[0].boundary_kind, "eof");
    }

    #[test]
    fn eof_with_open_tint_is_flagged_like_a_boundary() {
        let script = "tint(\"bg\", [0.55,0.6,0.68], 0)\n";
        let issues = check_lifecycle_issues(script);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].call_kind, LifecycleCallKind::Tint);
        assert_eq!(issues[0].boundary_kind, "eof");
    }

    fn wrap_in_sequence(body: &str) -> String {
        format!("<|\n{}\n|>\n\n{body}\n\n<|\n{}\n|>\n", world_state::SEQUENCE_BEGIN_MARKER, world_state::SEQUENCE_END_MARKER)
    }

    #[test]
    fn find_marker_lines_finds_all_occurrences_in_order() {
        let script = "alpha\nbeta\nalpha\n";
        let offsets = find_marker_lines(script, "alpha");
        assert_eq!(offsets.len(), 2);
        assert!(offsets[0] < offsets[1]);
    }

    #[test]
    fn find_marker_lines_returns_empty_for_absent_marker() {
        let script = "nothing here\n";
        assert!(find_marker_lines(script, world_state::SEQUENCE_BEGIN_MARKER).is_empty());
    }

    #[test]
    fn lifecycle_tracker_ignores_bg_switches_inside_sequence() {
        let body = "show(\"bg\", \"cgs/a\")\nshow(\"bg\", \"cgs/b\")\nshow(\"bg\", \"cgs/c\")";
        let script = wrap_in_sequence(body);
        let issues = check_lifecycle_issues(&script);
        assert!(issues.iter().all(|issue| issue.boundary_kind != "show_switch"));
    }

    #[test]
    fn lifecycle_tracker_ignores_trans_calls_inside_sequence() {
        let body = "tint(\"yuki\", [0.3,0.3,0.3], 0)\ntrans_fade(\"cam\", \"cgs/a\", 0.2)\ntrans_fade(\"cam\", \"cgs/b\", 0.2)";
        let script = wrap_in_sequence(body);
        let issues = check_lifecycle_issues(&script);
        assert!(issues.iter().all(|issue| issue.boundary_kind != "trans_fade"));
    }

    #[test]
    fn lifecycle_tracker_flushes_at_sequence_end_not_individual_switches() {
        let body = "tint(\"yuki\", [0.3,0.3,0.3], 0)\ntrans_fade(\"cam\", \"cgs/a\", 0.2)";
        let script = wrap_in_sequence(body);
        let issues = check_lifecycle_issues(&script);
        let tint_issues: Vec<_> = issues.iter().filter(|issue| issue.call_kind == LifecycleCallKind::Tint).collect();
        assert_eq!(tint_issues.len(), 1);
        assert_eq!(tint_issues[0].boundary_kind, "sequence_end");
    }

    #[test]
    fn lifecycle_tracker_still_flushes_normally_outside_sequence_spans() {
        let script = "tint(\"yuki\", [0.3,0.3,0.3], 0)\ntrans_fade(\"cam\", \"backgrounds/toilet\", 2)\n";
        let issues = check_lifecycle_issues(script);
        let tint_issues: Vec<_> = issues.iter().filter(|issue| issue.call_kind == LifecycleCallKind::Tint).collect();
        assert_eq!(tint_issues.len(), 1);
        assert_eq!(tint_issues[0].boundary_kind, "trans_fade");
    }

    #[test]
    fn lifecycle_tracker_resumes_normal_boundary_detection_after_sequence_end() {
        let seq_body = "show(\"bg\", \"cgs/a\")\nshow(\"bg\", \"cgs/b\")";
        let script = format!(
            "{}tint(\"yuki\", [0.3,0.3,0.3], 0)\ntrans_fade(\"cam\", \"backgrounds/toilet\", 2)\n",
            wrap_in_sequence(seq_body)
        );
        let issues = check_lifecycle_issues(&script);
        let tint_issues: Vec<_> = issues.iter().filter(|issue| issue.call_kind == LifecycleCallKind::Tint).collect();
        assert_eq!(tint_issues.len(), 1);
        assert_eq!(tint_issues[0].boundary_kind, "trans_fade");
    }

    #[test]
    fn lifecycle_tracker_last_bg_fg_path_keeps_updating_during_suppressed_sequence() {
        let seq_body = "show(\"bg\", \"cgs/a\")\nshow(\"bg\", \"cgs/b\")";
        let script = format!("{}show(\"bg\", \"cgs/b\")\n", wrap_in_sequence(seq_body));
        // The last bg shown inside the sequence is "cgs/b" - showing the *same* path again right
        // after the sequence ends must NOT be treated as a switch.
        let issues = check_lifecycle_issues(&script);
        assert!(issues.iter().all(|issue| issue.boundary_kind != "show_switch"));
    }

    #[test]
    fn flags_unclosed_performance_sequence() {
        let script = format!("<|\n{}\n|>\nraw content\n", world_state::SEQUENCE_BEGIN_MARKER);
        let issues = check_unclosed_sequences(&script);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].opened_at_line, 2);
    }

    #[test]
    fn closed_sequence_yields_no_unclosed_sequence_issue() {
        let script = wrap_in_sequence("raw content");
        assert!(check_unclosed_sequences(&script).is_empty());
    }

    #[test]
    fn multiple_sequences_in_one_script_all_correctly_paired() {
        let script = format!("{}\n{}", wrap_in_sequence("first"), wrap_in_sequence("second"));
        assert!(check_unclosed_sequences(&script).is_empty());
    }

    #[test]
    fn unclosed_sequence_still_gets_eof_lifecycle_flush_as_fallback() {
        let script = format!("<|\n{}\n|>\ntint(\"yuki\", [0.3,0.3,0.3], 0)\n", world_state::SEQUENCE_BEGIN_MARKER);
        let issues = check_lifecycle_issues(&script);
        assert!(issues.iter().any(|issue| issue.call_kind == LifecycleCallKind::Tint && issue.boundary_kind == "eof"));
        assert_eq!(check_unclosed_sequences(&script).len(), 1);
    }

    #[test]
    fn format_unclosed_sequence_issues_mentions_opened_line() {
        let message = format_unclosed_sequence_issues(&[UnclosedSequenceIssue { opened_at_line: 7 }]);
        assert!(message.contains('7'));
    }

    #[test]
    fn formats_lifecycle_show_issue_with_line_and_object() {
        let issues = vec![LifecycleIssue {
            object: "yuki".to_string(),
            call_kind: LifecycleCallKind::Show,
            opened_at_line: 3,
            boundary_line: 9,
            boundary_kind: "trans_fade",
            auto_fix: None,
        }];
        let message = format_lifecycle_issues(&issues);
        assert!(message.contains("第3行"));
        assert!(message.contains("第9行"));
        assert!(message.contains("yuki"));
        assert!(message.contains("trans_fade"));
    }

    #[test]
    fn accepts_node_closed_via_is_end() {
        let script = "@<|\nlabel(\"ch1_room\", \"宿舍\")\n|>\n<|\nshow(\"bg\", \"backgrounds/room\")\n|>\n你好。\n@<| is_end() |>\n";
        assert!(check_terminal_closure(script).is_empty());
    }

    #[test]
    fn accepts_node_closed_via_jump_to() {
        let script = "@<|\nlabel(\"ch1_room\", \"宿舍\")\n|>\n你好。\n@<| jump_to(\"ch2\") |>\n";
        assert!(check_terminal_closure(script).is_empty());
    }

    #[test]
    fn accepts_node_closed_via_branch() {
        let script = "@<|\nlabel(\"ch1_room\", \"宿舍\")\n|>\n你好。\n@<|\nbranch([\n    { dest=\"node_a\", text=\"A\" },\n])\n|>\n";
        assert!(check_terminal_closure(script).is_empty());
    }

    #[test]
    fn flags_last_node_with_no_closer() {
        let script = "@<|\nlabel(\"ch1_room\", \"宿舍\")\n|>\n<|\nshow(\"bg\", \"backgrounds/room\")\n|>\n你好。\n";
        let issues = check_terminal_closure(script);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].node_label, Some("ch1_room".to_string()));
        assert_eq!(issues[0].label_line, 2);
        assert_eq!(issues[0].last_content_line, 7);
    }

    #[test]
    fn flags_single_implicit_node_script_with_no_label_at_all_and_no_closer() {
        let script = "<|\nshow(\"bg\", \"backgrounds/room\")\n|>\n你好。\n";
        let issues = check_terminal_closure(script);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].node_label, None);
    }

    #[test]
    fn accepts_multi_node_script_where_only_last_node_matters() {
        // ch1_room has no explicit closer, but it doesn't need one - the next node's label() call
        // is itself the eager block that flushes ch1_room's trailing dialogue.
        let script = "@<|\nlabel(\"ch1_room\", \"宿舍\")\n|>\n你好。\n\n@<|\nlabel(\"ch1_hall\", \"走廊\")\n|>\n再见。\n@<| is_end() |>\n";
        assert!(check_terminal_closure(script).is_empty());
    }

    #[test]
    fn closure_call_starts_do_not_match_inside_longer_identifiers() {
        let starts = find_call_starts("sub_label(1)\nlabel(\"ch1\")\n", "label");
        assert_eq!(starts.len(), 1);
    }

    #[test]
    fn formats_terminal_closure_issue_with_suggested_fix() {
        let issues = vec![TerminalClosureIssue {
            node_label: Some("ch1_room".to_string()),
            label_line: 2,
            last_content_line: 7,
        }];
        let message = format_terminal_closure_issues(&issues);
        assert!(message.contains("ch1_room"));
        assert!(message.contains("第7行"));
        assert!(message.contains("is_end()"));
    }

    // --- position-conflict auto-fix ---

    #[test]
    fn inserts_fade_exit_when_different_character_placed_at_same_slot() {
        // xiben is placed center (x=0), then qianye is placed center (x=0) → xiben should exit.
        let script = "<|\nshow(\"xiben\", \"normal\", [0, 0, 0.5, 0, 0])\n|>\n台词A。\n\n<|\nshow(\"qianye\", \"normal\", [0, 0, 0.5, 0, 0])\n|>\n台词B。\n\n@<| is_end() |>\n";
        let patched = apply_position_conflict_fixes(script);
        assert!(patched.contains("hide(\"xiben\")"), "should have inserted hide for previous occupant");
        assert!(patched.contains("tint(\"xiben\", [1,1,1,0], 0.3)"), "should tint to transparent first");
        // The new show must still be present.
        assert!(patched.contains("show(\"qianye\""));
    }

    #[test]
    fn no_conflict_when_same_character_moved_within_same_slot() {
        // xiben stays at center (different pose) - no conflict.
        let script = "<|\nshow(\"xiben\", \"normal\", [0, 0, 0.5, 0, 0])\n|>\n台词A。\n\n<|\nshow(\"xiben\", \"happy\", [0, 0, 0.5, 0, 0])\n|>\n台词B。\n\n@<| is_end() |>\n";
        let patched = apply_position_conflict_fixes(script);
        assert!(!patched.contains("hide(\"xiben\")"));
    }

    #[test]
    fn no_conflict_after_explicit_hide_frees_slot() {
        // xiben is shown center, then explicitly hidden, then qianye is placed center - no conflict.
        let script = "<|\nshow(\"xiben\", \"normal\", [0, 0, 0.5, 0, 0])\n|>\n台词A。\n\n<|\nhide(\"xiben\")\nshow(\"qianye\", \"normal\", [0, 0, 0.5, 0, 0])\n|>\n台词B。\n\n@<| is_end() |>\n";
        let patched = apply_position_conflict_fixes(script);
        // No auto-inserted hide should appear before the show of qianye (the explicit hide above is enough).
        let qianye_pos = patched.find("show(\"qianye\"").unwrap();
        let before_qianye = &patched[..qianye_pos];
        // The only hide(xiben) present should be the original explicit one, not an auto-inserted duplicate.
        assert_eq!(before_qianye.matches("hide(\"xiben\")").count(), 1);
    }

    #[test]
    fn left_and_right_slots_are_independent() {
        // xiben left (x=-2), ergong right (x=2) - realistic positions, no conflict.
        let script = "<|\nshow(\"xiben\", \"normal\", [-2, 0, 0.53, 0, 0])\nshow(\"ergong\", \"normal\", [2, 0, 0.53, 0, 0])\n|>\n台词。\n\n@<| is_end() |>\n";
        let patched = apply_position_conflict_fixes(script);
        assert!(!patched.contains("hide(\"xiben\")"));
        assert!(!patched.contains("hide(\"ergong\")"));
    }
}
