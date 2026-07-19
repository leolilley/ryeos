#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ListItem<T> {
    pub(crate) id: String,
    pub(crate) search_text: String,
    pub(crate) value: T,
}

impl<T> ListItem<T> {
    pub(crate) fn new(id: impl Into<String>, search_text: impl Into<String>, value: T) -> Self {
        Self {
            id: id.into(),
            search_text: search_text.into(),
            value,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ListState<T> {
    items: Vec<ListItem<T>>,
    filter: String,
    visible: Vec<usize>,
    selected: Option<usize>,
    offset: usize,
    viewport_rows: usize,
}

impl<T> ListState<T> {
    pub(crate) fn new(items: Vec<ListItem<T>>, viewport_rows: usize) -> Self {
        let visible = (0..items.len()).collect::<Vec<_>>();
        let selected = (!visible.is_empty()).then_some(0);
        let mut state = Self {
            items,
            filter: String::new(),
            visible,
            selected,
            offset: 0,
            viewport_rows: viewport_rows.max(1),
        };
        state.keep_selected_visible();
        state
    }

    pub(crate) fn replace_items(&mut self, items: Vec<ListItem<T>>) {
        let selected_id = self.selected().map(|item| item.id.clone());
        self.items = items;
        self.rebuild_visible(selected_id);
    }

    pub(crate) fn set_filter(&mut self, filter: impl Into<String>) {
        let selected_id = self.selected().map(|item| item.id.clone());
        self.filter = filter.into();
        self.rebuild_visible(selected_id);
    }

    fn rebuild_visible(&mut self, selected_id: Option<String>) {
        let terms = self
            .filter
            .split_whitespace()
            .map(str::to_lowercase)
            .collect::<Vec<_>>();
        self.visible = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                let haystack = item.search_text.to_lowercase();
                terms
                    .iter()
                    .all(|term| haystack.contains(term))
                    .then_some(index)
            })
            .collect();
        self.selected = selected_id
            .and_then(|id| {
                self.visible
                    .iter()
                    .position(|index| self.items[*index].id == id)
            })
            .or_else(|| (!self.visible.is_empty()).then_some(0));
        self.offset = 0;
        self.keep_selected_visible();
    }

    pub(crate) fn set_viewport_rows(&mut self, rows: usize) {
        self.viewport_rows = rows.max(1);
        self.keep_selected_visible();
    }

    pub(crate) fn selected(&self) -> Option<&ListItem<T>> {
        self.selected
            .and_then(|selected| self.visible.get(selected))
            .map(|index| &self.items[*index])
    }

    pub(crate) fn find(
        &self,
        mut predicate: impl FnMut(&ListItem<T>) -> bool,
    ) -> Option<&ListItem<T>> {
        self.items.iter().find(|item| predicate(item))
    }

    pub(crate) fn selected_visible_index(&self) -> Option<usize> {
        self.selected
    }

    pub(crate) fn visible_window(&self) -> impl Iterator<Item = (usize, &ListItem<T>)> {
        self.visible
            .iter()
            .enumerate()
            .skip(self.offset)
            .take(self.viewport_rows)
            .map(|(visible_index, item_index)| (visible_index, &self.items[*item_index]))
    }

    pub(crate) fn visible_len(&self) -> usize {
        self.visible.len()
    }

    pub(crate) fn next(&mut self) {
        self.move_by(1);
    }

    pub(crate) fn previous(&mut self) {
        self.move_by(-1);
    }

    pub(crate) fn page_down(&mut self) {
        self.move_by(self.viewport_rows as isize);
    }

    pub(crate) fn page_up(&mut self) {
        self.move_by(-(self.viewport_rows as isize));
    }

    pub(crate) fn first(&mut self) {
        if !self.visible.is_empty() {
            self.selected = Some(0);
            self.keep_selected_visible();
        }
    }

    pub(crate) fn last(&mut self) {
        if !self.visible.is_empty() {
            self.selected = Some(self.visible.len() - 1);
            self.keep_selected_visible();
        }
    }

    fn move_by(&mut self, amount: isize) {
        let Some(selected) = self.selected else {
            return;
        };
        let last = self.visible.len().saturating_sub(1) as isize;
        self.selected = Some((selected as isize + amount).clamp(0, last) as usize);
        self.keep_selected_visible();
    }

    fn keep_selected_visible(&mut self) {
        let Some(selected) = self.selected else {
            self.offset = 0;
            return;
        };
        if selected < self.offset {
            self.offset = selected;
        } else if selected >= self.offset + self.viewport_rows {
            self.offset = selected + 1 - self.viewport_rows;
        }
        self.offset = self
            .offset
            .min(self.visible.len().saturating_sub(self.viewport_rows));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> ListState<&'static str> {
        ListState::new(
            vec![
                ListItem::new("execute", "execute run item", "execute"),
                ListItem::new("fetch", "fetch resolve item", "fetch"),
                ListItem::new("graph", "graph validate check", "graph"),
            ],
            2,
        )
    }

    #[test]
    fn filtering_matches_all_terms_and_preserves_selection() {
        let mut list = fixture();
        list.next();
        list.set_filter("resolve fetch");
        assert_eq!(list.selected().map(|item| item.id.as_str()), Some("fetch"));
        list.set_filter("");
        assert_eq!(list.selected().map(|item| item.id.as_str()), Some("fetch"));
    }

    #[test]
    fn empty_results_have_no_selection() {
        let mut list = fixture();
        list.set_filter("missing");
        assert_eq!(list.visible_len(), 0);
        assert!(list.selected().is_none());
        list.next();
        assert!(list.selected().is_none());
    }

    #[test]
    fn page_movement_is_bounded() {
        let mut list = fixture();
        list.page_down();
        assert_eq!(list.selected_visible_index(), Some(2));
        list.page_down();
        assert_eq!(list.selected_visible_index(), Some(2));
        list.page_up();
        assert_eq!(list.selected_visible_index(), Some(0));
    }
}
