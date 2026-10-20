//! Principal-owned saved Surface data. These records contain logical origins
//! and ordered compositions, never credentials, live mounts or binding grants.
use super::view_sets::{ParticularProjectRef, SavedViewSetTemplate};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReusableSurfaceRecord {
    pub id: String,
    pub title: String,
    pub origin: SavedSurfaceOrigin,
    pub view_sets: Vec<SavedViewSetTemplate>,
    /// Exact independent logical origin for every tab, supplied anew on open.
    pub tab_origins: std::collections::BTreeMap<String, SavedSurfaceOrigin>,
    pub active_view_set_id: String,
}

/// References canonical particular-set records in the same principal library.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ParticularSurfaceRecord {
    pub id: String,
    pub title: String,
    pub origin: SavedSurfaceOrigin,
    pub view_set_ids: Vec<String>,
    pub active_view_set_id: String,
}

/// Explicit original admitted origin, independently of individual tab subjects.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SavedSurfaceOrigin {
    pub surface_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ParticularProjectRef>,
}

fn identity(id: &str, title: &str) -> bool {
    !id.is_empty()
        && id.len() <= super::view_sets::MAX_SAVED_VIEW_SET_ID_BYTES
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        && !title.trim().is_empty()
        && title.len() <= super::view_sets::MAX_VIEW_SET_LABEL_BYTES
        && !title.chars().any(char::is_control)
}

pub fn validate_saved_surface_libraries(
    reusable: &[ReusableSurfaceRecord],
    particular: &[ParticularSurfaceRecord],
    particular_sets: &[super::view_sets::ParticularViewSetResume],
) -> Result<(), String> {
    use super::view_sets::*;
    validate_particular_view_set_resumes(particular_sets)?;
    if reusable.len() > MAX_SAVED_VIEW_SETS
        || particular.len() > MAX_PARTICULAR_VIEW_SETS
        || serde_json::to_vec(reusable)
            .map_err(|e| e.to_string())?
            .len()
            > MAX_SAVED_VIEW_SET_LIBRARY_BYTES
        || serde_json::to_vec(particular)
            .map_err(|e| e.to_string())?
            .len()
            > MAX_PARTICULAR_VIEW_SET_LIBRARY_BYTES
    {
        return Err("saved Surface library exceeds existing library bounds".into());
    }
    let origin_valid = |origin: &SavedSurfaceOrigin| {
        let reference = origin.surface_ref.strip_prefix("surface:");
        reference.is_some_and(|reference| {
            !reference.is_empty()
                && reference.trim() == reference
                && !reference.chars().any(char::is_control)
        }) && origin.project.as_ref().is_none_or(|project| {
            !project.local_id.is_empty()
                && project.local_id.len() <= MAX_PROJECT_LOCAL_ID_BYTES
                && project
                    .local_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':'))
        })
    };
    let mut ids = std::collections::BTreeSet::new();
    for record in reusable {
        if !identity(&record.id, &record.title)
            || !origin_valid(&record.origin)
            || !ids.insert(&record.id)
            || record.view_sets.is_empty()
            || record.view_sets.len() > MAX_VIEW_SETS
            || !record
                .view_sets
                .iter()
                .any(|set| set.id == record.active_view_set_id)
        {
            return Err("reusable Surface identity, origin or active tab is invalid".into());
        }
        validate_saved_view_set_templates(&record.view_sets)?;
        if record
            .tab_origins
            .keys()
            .collect::<std::collections::BTreeSet<_>>()
            != record
                .view_sets
                .iter()
                .map(|tab| &tab.id)
                .collect::<std::collections::BTreeSet<_>>()
            || record
                .tab_origins
                .values()
                .any(|origin| !origin_valid(origin))
        {
            return Err("reusable Surface must retain an exact valid origin for every tab".into());
        }
        for set in &record.view_sets {
            for relationship in &set.relationships {
                if let SavedViewSelectionSource::FollowSet { saved_view_set_id } =
                    &relationship.source
                    && !record
                        .view_sets
                        .iter()
                        .any(|set| &set.id == saved_view_set_id)
                {
                    return Err("reusable Surface follower leaves its saved collection".into());
                }
            }
        }
    }
    ids.clear();
    let known = particular_sets
        .iter()
        .map(|set| set.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for record in particular {
        let mut tabs = std::collections::BTreeSet::new();
        if !identity(&record.id, &record.title)
            || !origin_valid(&record.origin)
            || !ids.insert(&record.id)
            || record.view_set_ids.is_empty()
            || record.view_set_ids.len() > MAX_VIEW_SETS
            || !record.view_set_ids.contains(&record.active_view_set_id)
            || record
                .view_set_ids
                .iter()
                .any(|id| !known.contains(id.as_str()) || !tabs.insert(id))
        {
            return Err(
                "particular Surface must name unique existing ordered tabs and its active tab"
                    .into(),
            );
        }
        for set in particular_sets
            .iter()
            .filter(|set| record.view_set_ids.contains(&set.id))
        {
            for relationship in &set.relationships {
                if let SavedViewSelectionSource::FollowSet { saved_view_set_id } =
                    &relationship.source
                    && !record.view_set_ids.contains(saved_view_set_id)
                {
                    return Err("particular Surface follower leaves its saved collection".into());
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reusable_tab_origins_are_mandatory_exact_and_authority_free() {
        let value = json!({"id":"collection","title":"Collection",
            "origin":{"surface_ref":"surface:test/origin"},
            "view_sets":[{"id":"one","name":"One","composition":{"schema":2,"id":"one","title":"One","root":null},"relationships":[]}],
            "tab_origins":{"one":{"surface_ref":"surface:test/tab","project":{"local_id":"registered-one"}}},
            "active_view_set_id":"one"});
        let record: ReusableSurfaceRecord = serde_json::from_value(value.clone()).unwrap();
        validate_saved_surface_libraries(std::slice::from_ref(&record), &[], &[]).unwrap();
        let mut missing = value.clone();
        missing.as_object_mut().unwrap().remove("tab_origins");
        assert!(serde_json::from_value::<ReusableSurfaceRecord>(missing).is_err());
        let mut incomplete = record.clone();
        incomplete.tab_origins.clear();
        assert!(validate_saved_surface_libraries(&[incomplete], &[], &[]).is_err());
        let mut extra = record;
        extra
            .tab_origins
            .insert("foreign".into(), extra.origin.clone());
        assert!(validate_saved_surface_libraries(&[extra], &[], &[]).is_err());
        let mut forbidden = value;
        forbidden["tab_origins"]["one"]["binding_digest"] = json!("a".repeat(64));
        assert!(serde_json::from_value::<ReusableSurfaceRecord>(forbidden).is_err());
    }

    #[test]
    fn particular_followers_must_stay_inside_collection_and_records_validate_here() {
        let set = |id: &str, followed: &str| {
            serde_json::from_value::<super::super::view_sets::ParticularViewSetResume>(json!({
            "id":id,"name":id,"composition":{"schema":2,"id":id,"title":id,
                "root":{"type":"group","views":["view:test/detail"],"active":0}},
            "relationships":[{"mount":{"kind":"tile","index":0},"source":{"mode":"follow_set","saved_view_set_id":followed}}],
            "context":{"surface_ref":"surface:test/tab","project":{"local_id":"project-one"}}
        })).unwrap()
        };
        let record = ParticularSurfaceRecord {
            id: "collection".into(),
            title: "Collection".into(),
            origin: SavedSurfaceOrigin {
                surface_ref: "surface:test/origin".into(),
                project: None,
            },
            view_set_ids: vec!["one".into(), "two".into()],
            active_view_set_id: "one".into(),
        };
        let mut sets = vec![
            set("one", "two"),
            set("two", "one"),
            set("outside", "outside"),
        ];
        validate_saved_surface_libraries(&[], std::slice::from_ref(&record), &sets).unwrap();
        sets[0] = set("one", "outside");
        assert!(
            validate_saved_surface_libraries(&[], std::slice::from_ref(&record), &sets)
                .unwrap_err()
                .contains("leaves")
        );
        sets[0] = set("one", "two");
        sets[0].context.project = None;
        assert!(validate_saved_surface_libraries(&[], &[record], &sets).is_err());
    }
}
