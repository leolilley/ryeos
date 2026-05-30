//! Studio daemon DTOs.
//!
//! These structs model the JSON returned by the current daemon UI endpoints
//! without making those endpoint names part of the Studio product model.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioSnapshotDto {
    #[serde(default)]
    pub schema_version: String,
    #[serde(default)]
    pub generated_at: String,
    #[serde(default)]
    pub session: StudioSessionDto,
    #[serde(default)]
    pub local_node: StudioLocalNodeDto,
    #[serde(default)]
    pub project: Option<StudioProjectDto>,
    #[serde(default)]
    pub remotes: Vec<StudioRemoteDto>,
    #[serde(default)]
    pub threads: StudioThreadSummaryDto,
    #[serde(default)]
    pub schedules: StudioScheduleSummaryDto,
    #[serde(default)]
    pub gc: StudioGcSummaryDto,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioSessionDto {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub surface_ref: String,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub granted_caps: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioLocalNodeDto {
    #[serde(default)]
    pub identity: StudioIdentityDto,
    #[serde(default)]
    pub status: serde_json::Value,
    #[serde(default)]
    pub health: serde_json::Value,
    #[serde(default)]
    pub spaces: Vec<StudioSpaceDto>,
    #[serde(default)]
    pub bundles: Vec<StudioBundleDto>,
    #[serde(default)]
    pub services: Vec<StudioServiceDto>,
    #[serde(default)]
    pub verbs: Vec<StudioVerbAliasDto>,
    #[serde(default)]
    pub aliases: Vec<StudioVerbAliasDto>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioIdentityDto {
    #[serde(default)]
    pub principal_id: String,
    #[serde(default)]
    pub fingerprint: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioSpaceDto {
    #[serde(default)]
    pub space: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub path: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioBundleDto {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub path: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioServiceDto {
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub service_ref: String,
    #[serde(default)]
    pub availability: String,
    #[serde(default)]
    pub required_caps: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioVerbAliasDto {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioProjectDto {
    #[serde(default)]
    pub path: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioRemoteDto {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub principal_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioThreadSummaryDto {
    #[serde(default)]
    pub active_count: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioScheduleSummaryDto {
    #[serde(default)]
    pub total: usize,
    #[serde(default)]
    pub enabled: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioGcSummaryDto {
    #[serde(default)]
    pub running: bool,
    #[serde(default)]
    pub recent_events: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioItemsDto {
    #[serde(default)]
    pub schema_version: String,
    #[serde(default)]
    pub counts: StudioItemCountsDto,
    #[serde(default)]
    pub items: Vec<StudioItemDto>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioItemCountsDto {
    #[serde(default)]
    pub by_kind: BTreeMap<String, usize>,
    #[serde(default)]
    pub by_space: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioItemDto {
    #[serde(default)]
    pub canonical_ref: String,
    #[serde(default)]
    pub item_kind: String,
    #[serde(default)]
    pub bare_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub space: String,
    #[serde(default)]
    pub source_path: String,
    #[serde(default)]
    pub executable: bool,
    #[serde(default)]
    pub trust: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioThreadsDto {
    #[serde(default)]
    pub threads: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioSchedulesDto {
    #[serde(default)]
    pub schedules: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioGcStatusDto {
    #[serde(default)]
    pub running: bool,
    #[serde(default)]
    pub state: Option<serde_json::Value>,
    #[serde(default)]
    pub recent_events: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioFilesDto {
    #[serde(default)]
    pub root: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub entries: Vec<StudioFileEntryDto>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioFileEntryDto {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub is_dir: bool,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub modified: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioFileReadDto {
    #[serde(default)]
    pub root: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub size: usize,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub content: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioItemInspectionDto {
    #[serde(default)]
    pub item: StudioInspectedItemDto,
    #[serde(default)]
    pub raw: Option<StudioRawContentDto>,
    #[serde(default)]
    pub effective: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioInspectedItemDto {
    #[serde(default)]
    pub canonical_ref: String,
    #[serde(default)]
    pub item_kind: String,
    #[serde(default)]
    pub bare_id: String,
    #[serde(default)]
    pub source_path: String,
    #[serde(default)]
    pub space: String,
    #[serde(default)]
    pub executable: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioRawContentDto {
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub bytes: usize,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioThreadInspectionDto {
    #[serde(default)]
    pub thread: serde_json::Value,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    pub artifacts: Vec<serde_json::Value>,
    #[serde(default)]
    pub children: Vec<serde_json::Value>,
    #[serde(default)]
    pub facets: Option<serde_json::Value>,
    #[serde(default)]
    pub events: Vec<serde_json::Value>,
}
