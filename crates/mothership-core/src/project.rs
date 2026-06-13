use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, optional_fields = nullable)]
pub struct ProjectSummary {
    pub id: String,
    pub name: String,
    pub path: String,
    pub chat_count: i64,
    /// User-picked sidebar icon: `emoji:<char>` or `lucide:<id>`. `None` =>
    /// the default folder glyph.
    #[serde(default)]
    pub icon: Option<String>,
    /// User-picked accent for the icon tile (`#rrggbb`). `None` => theme default.
    #[serde(default)]
    pub icon_color: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub last_opened_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ProjectSnapshot {
    pub projects: Vec<ProjectSummary>,
    pub active_project_id: Option<String>,
}
