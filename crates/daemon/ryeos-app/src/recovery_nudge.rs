//! Process-local wake for the daemon's periodic recovery pass.
//!
//! A caller that leaves durable work for recovery to finish (a stop whose
//! SIGKILL was delivered but whose group is not yet observed empty) nudges the
//! pass instead of leaving it to the next scheduled tick. The nudge carries no
//! authority: recovery re-derives everything from durable rows.
//! `Notify::notify_one` retains one permit, so a nudge sent while a pass is
//! running is not lost.

use std::sync::OnceLock;

use tokio::sync::Notify;

fn global() -> &'static Notify {
    static NUDGE: OnceLock<Notify> = OnceLock::new();
    NUDGE.get_or_init(Notify::new)
}

/// Ask the periodic recovery pass to run now.
pub fn nudge() {
    global().notify_one();
}

/// Resolves when recovery has been nudged since the last wait.
pub async fn nudged() {
    global().notified().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_nudge_sent_before_the_wait_is_not_lost() {
        nudge();
        tokio::time::timeout(std::time::Duration::from_secs(1), nudged())
            .await
            .expect("stored nudge permit wakes the next wait");
    }
}
