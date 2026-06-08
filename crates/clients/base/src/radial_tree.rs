use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RadialTreeOptions {
    pub ring_gap: f32,
    pub passthrough_depth_step: f32,
    pub angle_start: f32,
    pub angle_end: f32,
}

impl Default for RadialTreeOptions {
    fn default() -> Self {
        Self {
            ring_gap: 1.35,
            passthrough_depth_step: 0.35,
            angle_start: -std::f32::consts::PI,
            angle_end: std::f32::consts::PI,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RadialTreeNode {
    pub path: Vec<String>,
    pub depth: u16,
    pub visual_depth: f32,
    pub angle: f32,
    pub angle_start: f32,
    pub angle_end: f32,
    pub radius: f32,
    pub position: [f32; 3],
    pub terminal: bool,
    pub child_count: u16,
}

#[derive(Debug, Default)]
struct TrieNode {
    children: BTreeMap<String, TrieNode>,
    terminal: bool,
    leaf_weight: f32,
}

pub fn layout_paths(paths: Vec<Vec<String>>) -> Vec<RadialTreeNode> {
    layout_paths_with_options(paths, RadialTreeOptions::default())
}

pub fn layout_paths_with_options(
    paths: Vec<Vec<String>>,
    options: RadialTreeOptions,
) -> Vec<RadialTreeNode> {
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
        0.0,
        options.angle_start,
        options.angle_end,
        options,
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
    visual_depth: f32,
    start_angle: f32,
    end_angle: f32,
    options: RadialTreeOptions,
    nodes: &mut Vec<RadialTreeNode>,
) {
    let depth = path.len() as u16;
    let angle = (start_angle + end_angle) * 0.5;
    let radius = visual_depth * options.ring_gap;
    nodes.push(RadialTreeNode {
        path: path.clone(),
        depth,
        visual_depth,
        angle,
        angle_start: start_angle,
        angle_end: end_angle,
        radius,
        position: [radius * angle.cos(), 0.0, radius * angle.sin()],
        terminal: node.terminal,
        child_count: node.children.len().min(u16::MAX as usize) as u16,
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
        let child_visual_depth = visual_depth + visual_depth_step(child, &options);
        collect_layout(
            child,
            child_path,
            child_visual_depth,
            cursor,
            cursor + span,
            options,
            nodes,
        );
        cursor += span;
    }
}

fn visual_depth_step(node: &TrieNode, options: &RadialTreeOptions) -> f32 {
    if !node.terminal && node.children.len() == 1 {
        options.passthrough_depth_step
    } else {
        1.0
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

    #[test]
    fn layout_is_independent_of_input_order() {
        let paths_a = vec![
            vec!["rye".into(), "core".into()],
            vec!["rye".into(), "file-system".into(), "read".into()],
        ];
        let paths_b = vec![paths_a[1].clone(), paths_a[0].clone()];

        let nodes_a = layout_paths(paths_a);
        let nodes_b = layout_paths(paths_b);

        assert_eq!(nodes_a, nodes_b);
    }

    #[test]
    fn compresses_single_child_passthrough_nodes() {
        let nodes = layout_paths_with_options(
            vec![vec![
                "some".into(),
                "folder".into(),
                "that".into(),
                "is".into(),
                "deep".into(),
            ]],
            RadialTreeOptions {
                ring_gap: 10.0,
                passthrough_depth_step: 0.25,
                ..Default::default()
            },
        );

        let some = node(&nodes, &["some"]);
        let folder = node(&nodes, &["some", "folder"]);
        let that = node(&nodes, &["some", "folder", "that"]);
        let is = node(&nodes, &["some", "folder", "that", "is"]);
        let deep = node(&nodes, &["some", "folder", "that", "is", "deep"]);

        assert_eq!(some.visual_depth, 0.25);
        assert_eq!(folder.visual_depth, 0.5);
        assert_eq!(that.visual_depth, 0.75);
        assert_eq!(is.visual_depth, 1.0);
        assert_eq!(deep.visual_depth, 2.0);
        assert_eq!(deep.radius, 20.0);
    }

    #[test]
    fn branch_and_terminal_nodes_take_full_spacing() {
        let nodes = layout_paths_with_options(
            vec![
                vec!["rye".into(), "core".into()],
                vec!["rye".into(), "file-system".into()],
            ],
            RadialTreeOptions {
                ring_gap: 10.0,
                passthrough_depth_step: 0.25,
                ..Default::default()
            },
        );

        let rye = node(&nodes, &["rye"]);
        let core = node(&nodes, &["rye", "core"]);
        let file_system = node(&nodes, &["rye", "file-system"]);

        assert_eq!(rye.child_count, 2);
        assert_eq!(rye.visual_depth, 1.0);
        assert_eq!(core.visual_depth, 2.0);
        assert_eq!(file_system.visual_depth, 2.0);
    }

    #[test]
    fn angular_spans_are_weighted_by_leaf_count() {
        let nodes = layout_paths(vec![
            vec!["a".into(), "one".into()],
            vec!["a".into(), "two".into()],
            vec!["b".into(), "only".into()],
        ]);

        let a = node(&nodes, &["a"]);
        let b = node(&nodes, &["b"]);
        let a_span = a.angle_end - a.angle_start;
        let b_span = b.angle_end - b.angle_start;

        assert!((a_span / b_span - 2.0).abs() < 0.001);
    }

    fn node<'a>(nodes: &'a [RadialTreeNode], path: &[&str]) -> &'a RadialTreeNode {
        nodes
            .iter()
            .find(|node| {
                node.path
                    .iter()
                    .map(String::as_str)
                    .eq(path.iter().copied())
            })
            .expect("node")
    }
}
