//! Layout-preference persistence vectors: the same scoped, debounced rules the
//! browser host follows, replayed against `LayoutPersistence` with a virtual
//! clock and a scripted store.

mod common;

use std::cell::RefCell;
use std::collections::HashMap;

use common::{load, name_of};
use ryeos_client_runtime::RuntimeError;
use ryeos_client_runtime::layout::LayoutPersistence;
use ryeos_client_runtime::platform::Store;
use serde_json::{Value, json};

#[derive(Default)]
struct ScriptedStore {
    entries: RefCell<HashMap<String, String>>,
    writes: RefCell<Vec<(String, String)>>,
    set_throws: bool,
}

impl Store for ScriptedStore {
    fn get(&self, key: &str) -> Option<String> {
        self.entries.borrow().get(key).cloned()
    }

    fn set(&self, key: &str, value: &str) -> Result<(), RuntimeError> {
        if self.set_throws {
            return Err(RuntimeError::Fatal("quota".to_owned()));
        }
        self.writes
            .borrow_mut()
            .push((key.to_owned(), value.to_owned()));
        self.entries
            .borrow_mut()
            .insert(key.to_owned(), value.to_owned());
        Ok(())
    }

    fn remove(&self, key: &str) {
        self.entries.borrow_mut().remove(key);
    }
}

#[test]
fn layout_preference_vectors() {
    let cases = load("layout-preferences");
    assert!(!cases.is_empty(), "no layout-preferences vectors");
    for case in &cases {
        let name = name_of(case);
        let options = &case["input"]["options"];
        let store = ScriptedStore {
            set_throws: options["set_throws"].as_bool().unwrap_or(false),
            ..ScriptedStore::default()
        };
        for (key, value) in case["input"]["initial_storage"]
            .as_object()
            .expect("storage")
        {
            store
                .entries
                .borrow_mut()
                .insert(key.clone(), value.as_str().expect("text").to_owned());
        }
        let current_throws = options["current_throws"].as_bool().unwrap_or(false);
        let mut persistence = LayoutPersistence::new(400);
        let mut errors: Vec<String> = Vec::new();
        let mut key: Option<String> = None;
        let mut current = String::new();
        let mut now = 0_u64;
        let mut observed: Vec<Value> = Vec::new();
        for step in case["input"]["steps"].as_array().expect("steps") {
            let op = step["op"].as_str().expect("op");
            let mut returns = Value::Null;
            match op {
                "set_key" => key = step["value"].as_str().map(str::to_owned),
                "set_current" => current = step["value"].as_str().expect("text").to_owned(),
                "observe" => {
                    let result = persistence.observe(
                        now,
                        key.clone(),
                        || {
                            if current_throws {
                                Err("boom".to_owned())
                            } else {
                                Ok(current.clone())
                            }
                        },
                        &store,
                    );
                    returns = json!(result);
                }
                "flush" => returns = json!(persistence.flush(&store)),
                "fire_timers" => {
                    now += 400;
                    persistence.write_due(now, &store);
                }
                other => panic!("{name}: unknown op {other}"),
            }
            errors.extend(persistence.take_errors());
            observed.push(json!({
                "op": op,
                "returns": returns,
                "writes_so_far": store.writes.borrow().len(),
                "pending_timers": persistence.pending_timers(),
            }));
        }

        let expected = &case["expected"];
        for (index, (got, want)) in observed
            .iter()
            .zip(expected["steps"].as_array().expect("steps"))
            .enumerate()
        {
            let op = want["op"].as_str().expect("op");
            assert_eq!(got["returns"], want["returns"], "{name} step {index} {op}");
            assert_eq!(
                got["writes_so_far"], want["writes_so_far"],
                "{name} step {index} {op}"
            );
            // The reference harness counts a fired timer until the next step
            // replaces it, so its count right after `fire_timers` is the
            // count before firing; every other step reports what is pending.
            if op != "fire_timers" {
                assert_eq!(
                    got["pending_timers"], want["pending_timers"],
                    "{name} step {index} {op}"
                );
            }
        }
        let writes: Vec<Value> = store
            .writes
            .borrow()
            .iter()
            .map(|(key, value)| json!([key, value]))
            .collect();
        assert_eq!(Value::Array(writes), expected["writes"], "{name}");
        assert_eq!(json!(errors), expected["errors"], "{name}");
        let delays: Vec<Value> = persistence
            .queued_delays_ms()
            .iter()
            .map(|delay| json!(delay))
            .collect();
        assert_eq!(Value::Array(delays), expected["timer_delays_ms"], "{name}");
        let storage: serde_json::Map<String, Value> = store
            .entries
            .borrow()
            .iter()
            .map(|(key, value)| (key.clone(), json!(value)))
            .collect();
        assert_eq!(Value::Object(storage), expected["final_storage"], "{name}");
    }
}
