//! The pictures the daemon granted this session: fetched with the session's
//! own transport from `GET /ui/art/<sha256>` and decoded by Rune's image
//! cache.
//!
//! A hash reaches this module only from the view model, where the daemon put
//! it after granting it to the session; the route refuses any other. A fetch
//! that fails leaves the picture absent and the surface drawn flat; nothing
//! stands in for it. Only a browser host fetches: the headless host renders
//! fixtures with no daemon and so no pictures.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use rune::{Image, ImageCache};
use ryeos_client_base::art::is_art_hash;

/// Largest picture the daemon serves; the transport refuses a longer body.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub const MAX_PICTURE_BYTES: usize = 2 * 1024 * 1024;

/// The route path of one picture, `None` for anything that is not a hash.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn art_path(sha256: &str) -> Option<String> {
    is_art_hash(sha256).then(|| format!("/ui/art/{sha256}"))
}

/// Bytes that arrived for a hash, or why they did not.
pub type Arrived = Rc<RefCell<Vec<(String, Result<Vec<u8>, String>)>>>;

#[derive(Default)]
pub struct Pictures {
    cache: Option<ImageCache>,
    wake: Option<Rc<dyn Fn()>>,
    arrived: Arrived,
    requested: HashSet<String>,
    images: HashMap<String, Image>,
}

impl Pictures {
    /// Give the picture store the app's waker: the image cache wakes the app
    /// when a decode finishes, and a fetch wakes it when bytes arrive.
    pub fn start(&mut self, waker: rune::runtime::Waker) {
        let wake_cache = waker.clone();
        self.cache = Some(ImageCache::new(wake_cache));
        self.wake = Some(Rc::new(move || waker()));
    }

    /// The hashes not yet requested, marked requested. The caller fetches
    /// each and reports through [`Pictures::arrived_sink`].
    pub fn newly_wanted<'a>(&mut self, hashes: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        hashes
            .into_iter()
            .filter(|hash| is_art_hash(hash) && self.requested.insert((*hash).to_owned()))
            .map(str::to_owned)
            .collect()
    }

    /// Where a fetch reports, and the waker it calls when it has.
    pub fn arrived_sink(&self) -> Option<(Arrived, Rc<dyn Fn()>)> {
        Some((self.arrived.clone(), self.wake.clone()?))
    }

    /// Decode what has arrived. Returns whether anything changed.
    pub fn take_arrivals(&mut self) -> bool {
        let arrived: Vec<_> = self.arrived.borrow_mut().drain(..).collect();
        let Some(cache) = &self.cache else {
            return false;
        };
        let mut changed = false;
        for (hash, outcome) in arrived {
            // A failed fetch is not asked for again: the surface stays flat,
            // and a daemon that refused a granted hash would only refuse it
            // again.
            if let Ok(bytes) = outcome {
                self.images.insert(hash, cache.load(bytes));
                changed = true;
            }
        }
        changed
    }

    /// The decoded picture for a hash, once ready.
    pub fn image(&self, sha256: &str) -> Option<&Image> {
        self.images
            .get(sha256)
            .filter(|image| image.error().is_none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_hash_has_a_route_and_each_hash_is_wanted_once() {
        assert_eq!(
            art_path(&"a".repeat(64)).as_deref(),
            Some(format!("/ui/art/{}", "a".repeat(64)).as_str())
        );
        for bad in ["", "../x", &"A".repeat(64), &"a".repeat(63)] {
            assert!(art_path(bad).is_none(), "{bad}");
        }
        let mut pictures = Pictures::default();
        let one = "a".repeat(64);
        let two = "b".repeat(64);
        assert_eq!(
            pictures.newly_wanted([one.as_str(), two.as_str(), "nope"]),
            vec![one.clone(), two.clone()]
        );
        assert!(pictures.newly_wanted([one.as_str()]).is_empty());
        assert!(pictures.image(&one).is_none());
    }
}
