use crate::BackendError;
use std::{fs, path::Path};

pub fn register_character(
    project_dir: &Path,
    node_name: &str,
    bind_name: &str,
    image_folder: &str,
) -> Result<bool, BackendError> {
    let game_tscn_path = project_dir.join("scene").join("game.tscn");
    let content = fs::read_to_string(&game_tscn_path)?;
    let bind_line = format!("_bindName = \"{bind_name}\"");
    if content.contains(&bind_line) {
        return Ok(false);
    }

    let newline = detect_newline(&content);
    let mut lines: Vec<String> = content.split('\n').map(|line| line.trim_end_matches('\r').to_string()).collect();

    let last_character_header = lines
        .iter()
        .enumerate()
        .rfind(|(_, line)| line.starts_with("[node ") && line.contains("parent=\"World/Characters\""))
        .map(|(index, _)| index)
        .ok_or_else(|| BackendError::message("未找到 parent=\"World/Characters\" 的角色节点块"))?;

    let mut insertion_index = last_character_header + 1;
    while insertion_index < lines.len() {
        let line = lines[insertion_index].trim();
        if line.is_empty() || line.starts_with("[node ") {
            break;
        }
        insertion_index += 1;
    }

    while insertion_index < lines.len() && lines[insertion_index].trim().is_empty() {
        lines.remove(insertion_index);
    }

    let new_block = [
        format!("[node name=\"{node_name}\" type=\"Node3D\" parent=\"World/Characters\"]"),
        "script = ExtResource(\"7_cmpsp\")".to_string(),
        format!("_bindName = \"{bind_name}\""),
        format!("_imageFolder = \"{image_folder}\""),
    ];

    let mut inserted_lines = Vec::with_capacity(6);
    inserted_lines.push(String::new());
    inserted_lines.extend(new_block);
    if insertion_index < lines.len() {
        inserted_lines.push(String::new());
    }

    lines.splice(insertion_index..insertion_index, inserted_lines);

    let updated = lines.join(newline);
    fs::write(game_tscn_path, updated)?;
    Ok(true)
}

fn detect_newline(content: &str) -> &str {
    if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const FIXTURE: &str = r#"[gd_scene load_steps=8 format=3 uid="uid://bc4sqnqyin56v"]

[ext_resource type="Script" path="res://nova/sources/scripts/graphics/CompositeSpriteController.cs" id="7_cmpsp"]

[node name="World" type="Node3D" parent="."]

[node name="Characters" type="Node3D" parent="World"]

[node name="Ergong" type="Node3D" parent="World/Characters"]
script = ExtResource("7_cmpsp")
_bindName = "ergong"
_imageFolder = "Ergong"

[node name="Gaotian" type="Node3D" parent="World/Characters"]
script = ExtResource("7_cmpsp")
_bindName = "gaotian"
_imageFolder = "Gaotian"

[node name="Avatar" type="Node" parent="."]
"#;

    fn write_fixture(dir: &Path) -> std::path::PathBuf {
        let scene_dir = dir.join("scene");
        fs::create_dir_all(&scene_dir).unwrap();
        let path = scene_dir.join("game.tscn");
        fs::write(&path, FIXTURE).unwrap();
        path
    }

    #[test]
    fn inserts_new_character_block_after_last_character_node() {
        let dir = std::env::temp_dir().join(format!("vvn_char_template_test_{}", std::process::id()));
        let path = write_fixture(&dir);

        let inserted = register_character(&dir, "Xiben", "xiben", "Xiben").unwrap();
        assert!(inserted);

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("[node name=\"Xiben\" type=\"Node3D\" parent=\"World/Characters\"]"));
        assert!(content.contains("_bindName = \"xiben\""));
        assert!(content.contains("_imageFolder = \"Xiben\""));
        // New block must land after Gaotian's block and before the unrelated Avatar node.
        let gaotian_pos = content.find("_bindName = \"gaotian\"").unwrap();
        let xiben_pos = content.find("_bindName = \"xiben\"").unwrap();
        let avatar_pos = content.find("name=\"Avatar\"").unwrap();
        assert!(gaotian_pos < xiben_pos);
        assert!(xiben_pos < avatar_pos);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn is_idempotent_on_repeated_calls() {
        let dir = std::env::temp_dir().join(format!("vvn_char_template_test_idem_{}", std::process::id()));
        write_fixture(&dir);

        let first = register_character(&dir, "Xiben", "xiben", "Xiben").unwrap();
        assert!(first);
        let content_after_first = fs::read_to_string(dir.join("scene").join("game.tscn")).unwrap();

        let second = register_character(&dir, "Xiben", "xiben", "Xiben").unwrap();
        assert!(!second);
        let content_after_second = fs::read_to_string(dir.join("scene").join("game.tscn")).unwrap();

        assert_eq!(content_after_first, content_after_second);

        fs::remove_dir_all(&dir).ok();
    }
}
