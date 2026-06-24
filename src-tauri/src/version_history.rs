use crate::BackendError;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const PREVIEW_LEN: usize = 120;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredVersion {
    content: String,
    #[serde(default)]
    summary: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionInfo {
    pub id: String,
    pub timestamp_ms: u128,
    pub preview: String,
    pub summary: String,
}

fn history_dir(project_dir: &Path, name: &str) -> PathBuf {
    project_dir.join("vvn_data").join("history").join(name)
}

fn now_id() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    format!("{nanos}")
}

/// Snapshots the *current* on-disk content of `<scenarios_dir>/name` into history before it gets
/// overwritten - used by the manual-save path, where the old content is still on disk at the
/// moment of the call. A no-op if the file doesn't exist yet (nothing to preserve) or is empty.
pub fn snapshot_before_write(scenario_path: &Path, project_dir: &Path, name: &str, summary: &str) -> Result<(), BackendError> {
    let Ok(current) = fs::read_to_string(scenario_path) else {
        return Ok(());
    };
    snapshot_content(project_dir, name, &current, summary)
}

/// Snapshots an explicit `content` string into history, tagged with `summary`. Used by the AI
/// generation commit path: the *original* pre-generation content is held in memory from before
/// the retry loop's draft writes started overwriting the file on disk, so by the time generation
/// succeeds and a summary is available, the old content can no longer be read back off disk - it
/// has to be passed in directly instead of re-read. A no-op if `content` is empty.
pub fn snapshot_content(project_dir: &Path, name: &str, content: &str, summary: &str) -> Result<(), BackendError> {
    if content.is_empty() {
        return Ok(());
    }

    let dir = history_dir(project_dir, name);
    fs::create_dir_all(&dir)?;
    let id = now_id();
    let stored = StoredVersion {
        content: content.to_string(),
        summary: summary.to_string(),
    };
    fs::write(dir.join(format!("{id}.json")), serde_json::to_vec(&stored)?)?;
    Ok(())
}

/// Reads one history entry off disk, handling both the current `<id>.json` format ({content,
/// summary}) and the legacy `<id>.txt` format (plain content, predates the `summary` field) -
/// older projects already have real `.txt` snapshots on disk and those shouldn't quietly vanish
/// from the version list just because the storage format moved on.
fn read_stored_version(dir: &Path, id: &str) -> Option<StoredVersion> {
    let json_path = dir.join(format!("{id}.json"));
    if let Ok(bytes) = fs::read(&json_path) {
        return serde_json::from_slice(&bytes).ok();
    }
    let txt_path = dir.join(format!("{id}.txt"));
    let content = fs::read_to_string(&txt_path).ok()?;
    Some(StoredVersion { content, summary: String::new() })
}

pub fn list_versions(project_dir: &Path, name: &str) -> Result<Vec<VersionInfo>, BackendError> {
    let dir = history_dir(project_dir, name);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut versions: Vec<VersionInfo> = fs::read_dir(&dir)
        .map_err(|error| BackendError::message(format!("无法读取 {}: {error}", dir.display())))?
        .filter_map(|entry| entry.ok())
        .filter(|entry| matches!(entry.path().extension().and_then(|ext| ext.to_str()), Some("json") | Some("txt")))
        .filter_map(|entry| {
            let id = entry.path().file_stem()?.to_str()?.to_string();
            let timestamp_ms = id.parse::<u128>().ok()? / 1_000_000;
            let stored = read_stored_version(&dir, &id)?;
            let preview: String = stored.content.chars().take(PREVIEW_LEN).collect();
            Some(VersionInfo { id, timestamp_ms, preview, summary: stored.summary })
        })
        .collect();

    versions.sort_by(|a, b| b.timestamp_ms.cmp(&a.timestamp_ms));
    Ok(versions)
}

pub fn read_version(project_dir: &Path, name: &str, version_id: &str) -> Result<String, BackendError> {
    if version_id.is_empty() || version_id.contains("..") || version_id.contains('/') || version_id.contains('\\') {
        return Err(BackendError::message(format!("非法的版本 id: {version_id}")));
    }
    let dir = history_dir(project_dir, name);
    read_stored_version(&dir, version_id)
        .map(|stored| stored.content)
        .ok_or_else(|| BackendError::message(format!("读取版本 {version_id} 失败: 文件不存在或无法解析")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_dir() -> PathBuf {
        std::env::temp_dir().join(format!("vvn_version_history_test_{}_{}", std::process::id(), now_id()))
    }

    #[test]
    fn snapshot_then_list_then_read_round_trips() {
        let project_dir = fixture_dir();
        fs::create_dir_all(project_dir.join("resources").join("scenarios")).unwrap();
        let scenario_path = project_dir.join("resources").join("scenarios").join("ch1.txt");
        fs::write(&scenario_path, "original content").unwrap();

        snapshot_before_write(&scenario_path, &project_dir, "ch1.txt", "").unwrap();
        let versions = list_versions(&project_dir, "ch1.txt").unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].preview, "original content");

        let restored = read_version(&project_dir, "ch1.txt", &versions[0].id).unwrap();
        assert_eq!(restored, "original content");

        fs::remove_dir_all(&project_dir).ok();
    }

    #[test]
    fn snapshot_is_noop_when_file_does_not_exist_yet() {
        let project_dir = fixture_dir();
        fs::create_dir_all(project_dir.join("resources").join("scenarios")).unwrap();
        let scenario_path = project_dir.join("resources").join("scenarios").join("new_file.txt");

        snapshot_before_write(&scenario_path, &project_dir, "new_file.txt", "").unwrap();
        let versions = list_versions(&project_dir, "new_file.txt").unwrap();
        assert!(versions.is_empty());

        fs::remove_dir_all(&project_dir).ok();
    }

    #[test]
    fn multiple_snapshots_are_ordered_newest_first() {
        let project_dir = fixture_dir();
        fs::create_dir_all(project_dir.join("resources").join("scenarios")).unwrap();
        let scenario_path = project_dir.join("resources").join("scenarios").join("ch1.txt");

        fs::write(&scenario_path, "version A").unwrap();
        snapshot_before_write(&scenario_path, &project_dir, "ch1.txt", "").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));

        fs::write(&scenario_path, "version B").unwrap();
        snapshot_before_write(&scenario_path, &project_dir, "ch1.txt", "").unwrap();

        let versions = list_versions(&project_dir, "ch1.txt").unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].preview, "version B");
        assert_eq!(versions[1].preview, "version A");

        fs::remove_dir_all(&project_dir).ok();
    }

    #[test]
    fn rejects_path_traversal_in_version_id() {
        let project_dir = fixture_dir();
        assert!(read_version(&project_dir, "ch1.txt", "../../etc").is_err());
    }

    #[test]
    fn legacy_txt_snapshot_still_shows_up_with_empty_summary() {
        let project_dir = fixture_dir();
        let dir = history_dir(&project_dir, "ch1.txt");
        fs::create_dir_all(&dir).unwrap();
        let id = now_id();
        fs::write(dir.join(format!("{id}.txt")), "legacy plain-text snapshot").unwrap();

        let versions = list_versions(&project_dir, "ch1.txt").unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].id, id);
        assert_eq!(versions[0].preview, "legacy plain-text snapshot");
        assert_eq!(versions[0].summary, "");

        let restored = read_version(&project_dir, "ch1.txt", &id).unwrap();
        assert_eq!(restored, "legacy plain-text snapshot");

        fs::remove_dir_all(&project_dir).ok();
    }

    #[test]
    fn snapshot_persists_summary_alongside_content() {
        let project_dir = fixture_dir();
        fs::create_dir_all(project_dir.join("resources").join("scenarios")).unwrap();
        let scenario_path = project_dir.join("resources").join("scenarios").join("ch1.txt");
        fs::write(&scenario_path, "original content").unwrap();

        snapshot_before_write(&scenario_path, &project_dir, "ch1.txt", "把背景色调调暗，加了雨声").unwrap();
        let versions = list_versions(&project_dir, "ch1.txt").unwrap();
        assert_eq!(versions[0].summary, "把背景色调调暗，加了雨声");

        fs::remove_dir_all(&project_dir).ok();
    }
}
