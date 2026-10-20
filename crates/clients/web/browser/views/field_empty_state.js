/** Describe only the Rust-projected source state of a zero-entity Field. */
export function fieldEmptyState(field) {
  if (field.entities.length !== 0) return null;
  const sources = field.sources;
  if (sources.some((source) => source.phase === "error" || source.error)) {
    return { tone: "error", title: "Source evidence failed", detail: "No entities are shown. Inspect Evidence for the source error." };
  }
  if (sources.some((source) => source.phase === "unresolved")) {
    return { tone: "pending", title: "Source unresolved", detail: "No entities are shown for the current subject selection." };
  }
  if (sources.some((source) => source.phase === "loading" || source.phase === "refreshing")) {
    return { tone: "pending", title: "Source evidence pending", detail: "No entities are shown while source evidence is loading or refreshing." };
  }
  if (sources.some((source) => source.truncated)) {
    return { tone: "bounded", title: "Bounded projection", detail: "No entities are shown in this bounded source response; retained evidence may extend beyond it." };
  }
  if (sources.length > 0 && sources.every((source) => source.phase === "ready")) {
    return { tone: "empty", title: "No entities in this projection", detail: "This does not establish that no retained run or result exists." };
  }
  return { tone: "unknown", title: "No entities projected", detail: "Source availability is not established here. Inspect Evidence for retained context." };
}
