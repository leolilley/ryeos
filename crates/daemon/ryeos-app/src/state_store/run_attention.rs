//! Failed runs that warrant the operator's attention.
use super::*;

/// Rows scanned per query. The scan is newest first, so a node with more
/// retained runs than this reports only the newest ones.
const MAX_RUN_ATTENTION_SCAN: usize = 2_000;

/// How a chain root came to exist, as far as the store records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunOrigin {
    /// The root is linked as a child of another thread (a graph step, a
    /// callback child), not launched on its own.
    pub child: bool,
    /// The root's admitted launch capsule carries a scheduled-fire context.
    pub scheduled: bool,
    /// A live follow waiter waits on this chain.
    pub followed: bool,
}

impl RunOrigin {
    /// A run the principal launched directly raises attention. A child or
    /// scheduled run raises it only while something follows it.
    pub fn raises_attention(&self) -> bool {
        self.followed || !(self.child || self.scheduled)
    }
}

/// The chain head of a failed run that raises attention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedRunAttention {
    pub chain_root_id: String,
    pub thread_id: String,
    pub status: String,
    pub item_ref: String,
    pub project_root: Option<String>,
    pub updated_at: String,
}

impl StateStore {
    /// Chain heads owned by `owner_principal` whose status is one of
    /// `statuses`, newest first, limited to runs the owner launched or follows.
    /// A superseded head (one with a continuation successor) is skipped, as is
    /// a chain whose root belongs to another principal. Which statuses count
    /// as failed is the caller's: the store attaches no meaning to them.
    pub fn failed_run_attention(
        &self,
        owner_principal: &str,
        project_root: Option<&std::path::Path>,
        statuses: &[String],
        limit: usize,
    ) -> Result<Vec<FailedRunAttention>> {
        if statuses.is_empty() {
            return Ok(Vec::new());
        }
        let filter = queries::ThreadListFilter {
            principal: Some(owner_principal.to_owned()),
            project_root: project_root.map(std::path::Path::to_path_buf),
            ..Default::default()
        };
        let heads =
            self.list_threads_query(MAX_RUN_ATTENTION_SCAN, &filter, queries::ThreadSort::Newest)?;
        let mut raised = Vec::new();
        for head in heads {
            if raised.len() >= limit {
                break;
            }
            if head.successor_thread_id.is_some() || !statuses.contains(&head.status) {
                continue;
            }
            let root_owner = if head.thread_id == head.chain_root_id {
                head.requested_by.clone()
            } else {
                self.get_thread(&head.chain_root_id)?
                    .and_then(|root| root.requested_by)
            };
            if root_owner.as_deref() != Some(owner_principal) {
                continue;
            }
            let origin = self.run_origin(&head.chain_root_id)?;
            if !origin.raises_attention() {
                continue;
            }
            raised.push(FailedRunAttention {
                chain_root_id: head.chain_root_id,
                thread_id: head.thread_id,
                status: head.status,
                item_ref: head.item_ref,
                project_root: head.project_root,
                updated_at: head.updated_at,
            });
        }
        Ok(raised)
    }

    fn run_origin(&self, chain_root_id: &str) -> Result<RunOrigin> {
        let child = self.child_link_relation(chain_root_id)?.is_some();
        let followed = self
            .get_follow_waiter_by_child_chain(chain_root_id)?
            .is_some();
        let scheduled = self
            .admitted_launch_capsule(chain_root_id)?
            .is_some_and(|capsule| {
                capsule
                    .sealed_invocation
                    .get("scheduled_fire")
                    .is_some_and(|fire| !fire.is_null())
            });
        Ok(RunOrigin {
            child,
            scheduled,
            followed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_followed_child_or_scheduled_run_overrides_its_origin() {
        let origin = |child, scheduled, followed| RunOrigin {
            child,
            scheduled,
            followed,
        };
        assert!(origin(false, false, false).raises_attention());
        assert!(!origin(true, false, false).raises_attention());
        assert!(!origin(false, true, false).raises_attention());
        assert!(!origin(true, true, false).raises_attention());
        assert!(origin(true, false, true).raises_attention());
        assert!(origin(false, true, true).raises_attention());
    }
}
