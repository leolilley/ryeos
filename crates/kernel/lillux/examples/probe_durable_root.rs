use lillux::process_control::{decide_durable_root, RootStorageFacts};
fn main() {
    for p in std::env::args().skip(1) {
        let facts = RootStorageFacts::observe(std::path::Path::new(&p));
        println!("facts: {facts:#?}");
        match decide_durable_root(&facts) {
            Ok(e) => println!("DECISION {p}: ACCEPTED basis={}", e.basis),
            Err(r) => println!("DECISION {p}: REFUSED {r}"),
        }
    }
}
