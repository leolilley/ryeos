// Presentation-only grouping of Rust-projected preview ownership. This does
// not infer a comparison result or fetch missing preview content.
export function fieldPreviewGroups(field) {
  const entities = new Map(field.entities.map((entity) => [entity.id, entity]));
  const previewsFor = (entity) => {
    const retainedIds = new Set(entity.preview_ids);
    return field.previews.filter((preview) =>
      preview.entity_id === entity.id && retainedIds.has(preview.id));
  };
  const selected = field.selected ? entities.get(field.selected) : null;
  const compared = field.compare.flatMap((id) => {
    const entity = entities.get(id);
    return entity ? [{ entity, previews: previewsFor(entity) }] : [];
  });
  return {
    selectedPreviews: selected ? previewsFor(selected) : [],
    compared,
  };
}
