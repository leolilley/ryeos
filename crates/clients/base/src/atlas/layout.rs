use std::collections::BTreeMap;

const RING_GAP: f32 = 1.35;

#[derive(Debug, Clone, PartialEq)]
pub struct LayoutNode {
    pub path: Vec<String>,
    pub depth: u16,
    pub angle: f32,
    pub angle_start: f32,
    pub angle_end: f32,
    pub radius: f32,
    pub position: [f32; 3],
}

#[derive(Debug, Default)]
struct TrieNode {
    children: BTreeMap<String, TrieNode>,
    terminal: bool,
    leaf_weight: f32,
}

pub fn layout_paths(paths: Vec<Vec<String>>) -> Vec<LayoutNode> {
    let mut root = TrieNode::default();
    root.terminal = true;
    for path in paths {
        insert_path(&mut root, &path);
    }
    assign_leaf_weights(&mut root);
    let mut nodes = Vec::new();
    collect_layout(
        &root,
        Vec::new(),
        -std::f32::consts::PI,
        std::f32::consts::PI,
        &mut nodes,
    );
    nodes.sort_by(|a, b| a.path.cmp(&b.path));
    nodes
}

fn insert_path(root: &mut TrieNode, path: &[String]) {
    let mut node = root;
    for segment in path {
        node = node.children.entry(segment.clone()).or_default();
    }
    node.terminal = true;
}

fn assign_leaf_weights(node: &mut TrieNode) -> f32 {
    if node.children.is_empty() {
        node.leaf_weight = 1.0;
        return node.leaf_weight;
    }
    let child_weight: f32 = node.children.values_mut().map(assign_leaf_weights).sum();
    node.leaf_weight = child_weight.max(1.0);
    node.leaf_weight
}

fn collect_layout(
    node: &TrieNode,
    path: Vec<String>,
    start_angle: f32,
    end_angle: f32,
    nodes: &mut Vec<LayoutNode>,
) {
    let depth = path.len() as u16;
    let angle = (start_angle + end_angle) * 0.5;
    let radius = depth as f32 * RING_GAP;
    nodes.push(LayoutNode {
        path: path.clone(),
        depth,
        angle,
        angle_start: start_angle,
        angle_end: end_angle,
        radius,
        position: [radius * angle.cos(), 0.0, radius * angle.sin()],
    });

    if node.children.is_empty() {
        return;
    }

    let total_weight: f32 = node
        .children
        .values()
        .map(|child| child.leaf_weight)
        .sum::<f32>()
        .max(1.0);
    let mut cursor = start_angle;
    for (segment, child) in &node.children {
        let span = (end_angle - start_angle) * (child.leaf_weight / total_weight);
        let mut child_path = path.clone();
        child_path.push(segment.clone());
        collect_layout(child, child_path, cursor, cursor + span, nodes);
        cursor += span;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_contains_root_and_paths() {
        let nodes = layout_paths(vec![
            vec!["rye".into(), "core".into()],
            vec!["rye".into(), "file-system".into(), "read".into()],
        ]);
        assert!(nodes.iter().any(|node| node.path.is_empty()));
        assert!(nodes.iter().any(|node| node.path == vec!["rye", "core"]));
        assert!(nodes
            .iter()
            .any(|node| node.path == vec!["rye", "file-system", "read"]));
    }
}
