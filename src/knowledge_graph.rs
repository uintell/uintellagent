//! Shared knowledge-graph domain, persistence, indexing, layout, and analytics.
//!
//! Both terminal front ends use this module so a database record has one model
//! and graph behavior is tested independently from rendering and input handling.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub const DEFAULT_DATASET: &str = "default";
pub const RELATION_TYPES: [&str; 2] = ["relates_to", "proves"];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Fact {
    pub id: String,
    pub fact_type: String,
    pub content: String,
    pub source: String,
    pub confidence: f64,
    pub tags: Vec<String>,
    pub dataset: String,
    pub graph_x: f64,
    pub graph_y: f64,
    #[serde(default)]
    pub graph_pinned: bool,
    #[serde(default)]
    pub code_path: Option<String>,
    #[serde(default)]
    pub code_start_line: Option<usize>,
    #[serde(default)]
    pub code_end_line: Option<usize>,
    #[serde(default)]
    pub code_column: Option<usize>,
    #[serde(default)]
    pub code_symbol: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    pub timestamp: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Edge {
    pub id: String,
    pub relation_type: String,
    pub from_id: String,
    pub to_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Dataset {
    pub name: String,
    pub description: String,
}

impl Dataset {
    pub fn default_dataset() -> Self {
        Self {
            name: DEFAULT_DATASET.into(),
            description: "Default knowledge dataset".into(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct GraphSnapshot {
    pub facts: Vec<Fact>,
    pub edges: Vec<Edge>,
    pub datasets: Vec<Dataset>,
    pub total_facts: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct GraphLoadOptions {
    pub fact_limit: usize,
    pub fact_offset: usize,
    pub edge_limit_per_type: usize,
    pub dataset_limit: usize,
}

impl Default for GraphLoadOptions {
    fn default() -> Self {
        Self {
            fact_limit: 2_000,
            fact_offset: 0,
            edge_limit_per_type: 5_000,
            dataset_limit: 500,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GraphFilter {
    pub dataset: Option<String>,
    pub fact_type: Option<String>,
}

impl GraphFilter {
    pub fn from_labels(dataset: &str, fact_type: &str) -> Self {
        Self {
            dataset: (dataset != "all").then(|| dataset.to_string()),
            fact_type: (fact_type != "all").then(|| fact_type.to_string()),
        }
    }

    pub fn matches(&self, fact: &Fact) -> bool {
        self.dataset
            .as_ref()
            .is_none_or(|dataset| fact.dataset == *dataset)
            && self
                .fact_type
                .as_ref()
                .is_none_or(|fact_type| fact.fact_type == *fact_type)
    }

    pub fn indices(&self, facts: &[Fact]) -> Vec<usize> {
        facts
            .iter()
            .enumerate()
            .filter_map(|(index, fact)| self.matches(fact).then_some(index))
            .collect()
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GraphAnalytics {
    pub node_count: usize,
    pub edge_count: usize,
    pub degrees: HashMap<String, usize>,
    pub orphan_count: usize,
    pub duplicate_edge_count: usize,
    pub pinned_count: usize,
    pub average_degree: f64,
    pub maximum_degree: usize,
    pub type_counts: BTreeMap<String, usize>,
    pub dataset_counts: BTreeMap<String, usize>,
}

impl GraphAnalytics {
    pub fn compute(facts: &[Fact], edges: &[Edge]) -> Self {
        let mut degrees = HashMap::<String, usize>::new();
        let mut unique_edges = BTreeSet::new();
        let mut duplicate_edge_count = 0;
        for edge in edges {
            *degrees.entry(edge.from_id.clone()).or_default() += 1;
            *degrees.entry(edge.to_id.clone()).or_default() += 1;
            if !unique_edges.insert((
                edge.relation_type.as_str(),
                edge.from_id.as_str(),
                edge.to_id.as_str(),
            )) {
                duplicate_edge_count += 1;
            }
        }

        let mut type_counts = BTreeMap::new();
        let mut dataset_counts = BTreeMap::new();
        for fact in facts {
            *type_counts.entry(fact.fact_type.clone()).or_default() += 1;
            *dataset_counts.entry(fact.dataset.clone()).or_default() += 1;
        }

        let node_count = facts.len();
        Self {
            node_count,
            edge_count: edges.len(),
            orphan_count: facts
                .iter()
                .filter(|fact| degrees.get(&fact.id).copied().unwrap_or(0) == 0)
                .count(),
            duplicate_edge_count,
            pinned_count: facts.iter().filter(|fact| fact.graph_pinned).count(),
            average_degree: if node_count == 0 {
                0.0
            } else {
                edges.len() as f64 * 2.0 / node_count as f64
            },
            maximum_degree: degrees.values().copied().max().unwrap_or(0),
            degrees,
            type_counts,
            dataset_counts,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PositionUpdate {
    pub index: usize,
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct LayoutConfig {
    pub exact_layout_limit: usize,
    pub iterations: usize,
    pub min_coordinate: f64,
    pub max_coordinate: f64,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            exact_layout_limit: 220,
            iterations: 70,
            min_coordinate: 0.04,
            max_coordinate: 0.96,
        }
    }
}

/// Compute new positions without touching the database or UI state.
pub fn compute_layout(
    facts: &[Fact],
    edges: &[Edge],
    indices: &[usize],
    config: LayoutConfig,
) -> Vec<PositionUpdate> {
    if indices.is_empty() {
        return Vec::new();
    }

    let mut positions = indices
        .iter()
        .map(|index| (facts[*index].graph_x, facts[*index].graph_y))
        .collect::<Vec<_>>();
    let id_to_local = indices
        .iter()
        .enumerate()
        .map(|(local, index)| (facts[*index].id.as_str(), local))
        .collect::<HashMap<_, _>>();
    let local_edges = edges
        .iter()
        .filter_map(|edge| {
            Some((
                *id_to_local.get(edge.from_id.as_str())?,
                *id_to_local.get(edge.to_id.as_str())?,
            ))
        })
        .collect::<Vec<_>>();

    if indices.len() <= config.exact_layout_limit {
        let count = indices.len();
        let ideal = (1.0 / count.max(1) as f64).sqrt().max(0.04);
        for iteration in 0..config.iterations {
            let mut displacement = vec![(0.0, 0.0); count];
            for left in 0..count {
                for right in (left + 1)..count {
                    let dx = positions[left].0 - positions[right].0;
                    let dy = positions[left].1 - positions[right].1;
                    let distance = (dx * dx + dy * dy).sqrt().max(0.002);
                    let force = ideal * ideal / distance;
                    let fx = dx / distance * force;
                    let fy = dy / distance * force;
                    displacement[left].0 += fx;
                    displacement[left].1 += fy;
                    displacement[right].0 -= fx;
                    displacement[right].1 -= fy;
                }
            }
            for (from, to) in &local_edges {
                let dx = positions[*from].0 - positions[*to].0;
                let dy = positions[*from].1 - positions[*to].1;
                let distance = (dx * dx + dy * dy).sqrt().max(0.002);
                let force = distance * distance / ideal;
                let fx = dx / distance * force;
                let fy = dy / distance * force;
                displacement[*from].0 -= fx;
                displacement[*from].1 -= fy;
                displacement[*to].0 += fx;
                displacement[*to].1 += fy;
            }
            let progress = iteration as f64 / config.iterations.max(1) as f64;
            let temperature = 0.05 * (1.0 - progress).max(0.05);
            for local in 0..count {
                if facts[indices[local]].graph_pinned {
                    continue;
                }
                let (dx, dy) = displacement[local];
                let magnitude = (dx * dx + dy * dy).sqrt().max(0.001);
                positions[local].0 = (positions[local].0
                    + dx / magnitude * magnitude.min(temperature))
                .clamp(config.min_coordinate, config.max_coordinate);
                positions[local].1 = (positions[local].1
                    + dy / magnitude * magnitude.min(temperature))
                .clamp(config.min_coordinate, config.max_coordinate);
            }
        }
    } else {
        let count = indices.len();
        for (local, index) in indices.iter().enumerate() {
            if facts[*index].graph_pinned {
                continue;
            }
            let angle = std::f64::consts::TAU * local as f64 / count as f64;
            let ring = 0.28 + 0.09 * ((local / 40) as f64).min(2.0);
            positions[local] = (
                (0.5 + angle.cos() * ring).clamp(config.min_coordinate, config.max_coordinate),
                (0.5 + angle.sin() * ring).clamp(config.min_coordinate, config.max_coordinate),
            );
        }
    }

    indices
        .iter()
        .enumerate()
        .filter_map(|(local, index)| {
            (!facts[*index].graph_pinned).then_some(PositionUpdate {
                index: *index,
                x: positions[local].0,
                y: positions[local].1,
            })
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    pub center_x: f64,
    pub center_y: f64,
    pub zoom: f64,
}

impl Viewport {
    pub fn world_bounds(self, margin: f64) -> (f64, f64, f64, f64) {
        let half = 0.5 / self.zoom.max(0.01);
        (
            (self.center_x - half - margin).clamp(0.0, 1.0),
            (self.center_y - half - margin).clamp(0.0, 1.0),
            (self.center_x + half + margin).clamp(0.0, 1.0),
            (self.center_y + half + margin).clamp(0.0, 1.0),
        )
    }
}

/// Uniform-grid point index for sublinear viewport culling and hit testing.
#[derive(Clone, Debug)]
pub struct SpatialIndex {
    cells_per_axis: usize,
    buckets: Vec<Vec<usize>>,
    len: usize,
}

impl Default for SpatialIndex {
    fn default() -> Self {
        Self::new(32)
    }
}

impl SpatialIndex {
    pub fn new(cells_per_axis: usize) -> Self {
        let cells_per_axis = cells_per_axis.clamp(1, 256);
        Self {
            cells_per_axis,
            buckets: vec![Vec::new(); cells_per_axis * cells_per_axis],
            len: 0,
        }
    }

    pub fn rebuild(&mut self, facts: &[Fact]) {
        self.buckets.iter_mut().for_each(Vec::clear);
        self.len = facts.len();
        for (index, fact) in facts.iter().enumerate() {
            if fact.graph_x.is_finite() && fact.graph_y.is_finite() {
                let bucket = self.bucket_index(fact.graph_x, fact.graph_y);
                self.buckets[bucket].push(index);
            }
        }
    }

    pub fn query_viewport(&self, viewport: Viewport, margin: f64) -> Vec<usize> {
        let (min_x, min_y, max_x, max_y) = viewport.world_bounds(margin);
        self.query_rect(min_x, min_y, max_x, max_y)
    }

    pub fn query_rect(&self, min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Vec<usize> {
        if self.len == 0 || min_x > max_x || min_y > max_y {
            return Vec::new();
        }
        let min_cell_x = self.cell(min_x);
        let min_cell_y = self.cell(min_y);
        let max_cell_x = self.cell(max_x);
        let max_cell_y = self.cell(max_y);
        let mut matches = Vec::new();
        for y in min_cell_y..=max_cell_y {
            for x in min_cell_x..=max_cell_x {
                matches.extend_from_slice(&self.buckets[y * self.cells_per_axis + x]);
            }
        }
        matches.sort_unstable();
        matches
    }

    fn cell(&self, coordinate: f64) -> usize {
        ((coordinate.clamp(0.0, 1.0) * self.cells_per_axis as f64) as usize)
            .min(self.cells_per_axis - 1)
    }

    fn bucket_index(&self, x: f64, y: f64) -> usize {
        self.cell(y) * self.cells_per_axis + self.cell(x)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct GraphRepository;

impl GraphRepository {
    pub async fn query(sql: &str) -> Result<Vec<Value>, String> {
        crate::tools::graph::query_sql(sql).await
    }

    pub async fn transaction(sql: &str) -> Result<Vec<Value>, String> {
        let sql = sql.trim().trim_end_matches(';');
        if sql.is_empty() {
            return Ok(Vec::new());
        }
        Self::query(&format!("BEGIN TRANSACTION; {sql}; COMMIT TRANSACTION")).await
    }

    pub async fn load(options: GraphLoadOptions) -> Result<GraphSnapshot, String> {
        let fact_limit = options.fact_limit.clamp(1, 100_000);
        let fact_offset = options.fact_offset.min(10_000_000);
        let edge_limit = options.edge_limit_per_type.clamp(1, 500_000);
        let dataset_limit = options.dataset_limit.clamp(1, 10_000);
        let facts_sql = format!(
            "SELECT id, fact_type, content, source, confidence, tags, dataset, graph_x, graph_y, graph_pinned, code_path, code_start_line, code_end_line, code_column, code_symbol, run_id, timestamp FROM fact ORDER BY timestamp DESC LIMIT {fact_limit} START {fact_offset}"
        );
        let relates_sql = format!(
            "SELECT id, in AS from_id, out AS to_id FROM relates_to ORDER BY id LIMIT {edge_limit}"
        );
        let proves_sql = format!(
            "SELECT id, in AS from_id, out AS to_id FROM proves ORDER BY id LIMIT {edge_limit}"
        );
        let datasets_sql =
            format!("SELECT name, description FROM dataset ORDER BY name LIMIT {dataset_limit}");

        let (fact_rows, count_rows, relates_rows, proves_rows, dataset_rows) = tokio::try_join!(
            Self::query(&facts_sql),
            Self::query("SELECT count() FROM fact GROUP ALL"),
            Self::query(&relates_sql),
            Self::query(&proves_sql),
            Self::query(&datasets_sql),
        )?;

        let mut facts = parse_facts(&fact_rows);
        let total_facts = count_rows
            .first()
            .and_then(|row| row["result"].as_array())
            .and_then(|items| items.first())
            .and_then(|row| row["count"].as_u64())
            .unwrap_or(facts.len() as u64);
        let mut edges = parse_edges(&relates_rows, "relates_to");
        edges.extend(parse_edges(&proves_rows, "proves"));
        let mut datasets = parse_datasets(&dataset_rows);
        normalize_datasets(&mut datasets, &facts);

        let generated = initialize_missing_positions(&mut facts);
        if !generated.is_empty() {
            Self::persist_positions(&facts, &generated).await?;
        }

        Ok(GraphSnapshot {
            facts,
            edges,
            datasets,
            total_facts,
        })
    }

    pub async fn persist_position(fact: &Fact) -> Result<(), String> {
        if !valid_fact_id(&fact.id) {
            return Err(format!("invalid fact id: {}", fact.id));
        }
        Self::query(&format!(
            "UPDATE {} SET graph_x = {:.6}, graph_y = {:.6}, graph_pinned = {}, updated_at = time::now()",
            fact.id,
            fact.graph_x.clamp(0.0, 1.0),
            fact.graph_y.clamp(0.0, 1.0),
            fact.graph_pinned
        ))
        .await
        .map(|_| ())
    }

    pub async fn persist_positions(
        facts: &[Fact],
        updates: &[PositionUpdate],
    ) -> Result<(), String> {
        let mut statements = Vec::with_capacity(updates.len());
        for update in updates {
            let fact = facts
                .get(update.index)
                .ok_or_else(|| format!("position index {} is out of bounds", update.index))?;
            if !valid_fact_id(&fact.id) {
                return Err(format!("invalid fact id: {}", fact.id));
            }
            statements.push(format!(
                "UPDATE {} SET graph_x = {:.6}, graph_y = {:.6}, graph_pinned = {}, updated_at = time::now()",
                fact.id,
                update.x.clamp(0.0, 1.0),
                update.y.clamp(0.0, 1.0),
                fact.graph_pinned
            ));
        }
        Self::transaction(&statements.join("; ")).await.map(|_| ())
    }

    pub async fn relate(source: &str, relation_type: &str, target: &str) -> Result<(), String> {
        if !valid_fact_id(source) || !valid_fact_id(target) {
            return Err("relation endpoints must be valid fact record ids".into());
        }
        if !RELATION_TYPES.contains(&relation_type) {
            return Err(format!("unsupported relation type: {relation_type}"));
        }
        Self::query(&format!("RELATE {source}->{relation_type}->{target}"))
            .await
            .map(|_| ())
    }
}

fn parse_facts(rows: &[Value]) -> Vec<Fact> {
    rows.first()
        .and_then(|row| row["result"].as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|row| {
                    Some(Fact {
                        id: row["id"].as_str()?.into(),
                        fact_type: row["fact_type"].as_str()?.into(),
                        content: row["content"].as_str()?.into(),
                        source: row["source"].as_str().unwrap_or("?").into(),
                        confidence: row["confidence"].as_f64().unwrap_or(0.0),
                        tags: row["tags"]
                            .as_array()
                            .map(|tags| {
                                tags.iter()
                                    .filter_map(Value::as_str)
                                    .map(String::from)
                                    .collect()
                            })
                            .unwrap_or_default(),
                        dataset: row["dataset"].as_str().unwrap_or(DEFAULT_DATASET).into(),
                        graph_x: row["graph_x"].as_f64().unwrap_or(f64::NAN),
                        graph_y: row["graph_y"].as_f64().unwrap_or(f64::NAN),
                        graph_pinned: row["graph_pinned"].as_bool().unwrap_or(false),
                        code_path: row["code_path"].as_str().map(str::to_string),
                        code_start_line: row["code_start_line"].as_u64().map(|line| line as usize),
                        code_end_line: row["code_end_line"].as_u64().map(|line| line as usize),
                        code_column: row["code_column"].as_u64().map(|column| column as usize),
                        code_symbol: row["code_symbol"].as_str().map(str::to_string),
                        run_id: row["run_id"].as_str().map(str::to_string),
                        timestamp: row["timestamp"].as_str().unwrap_or("?").into(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_edges(rows: &[Value], relation_type: &str) -> Vec<Edge> {
    rows.first()
        .and_then(|row| row["result"].as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|row| {
                    Some(Edge {
                        id: row["id"].as_str()?.into(),
                        relation_type: relation_type.into(),
                        from_id: row["from_id"].as_str()?.into(),
                        to_id: row["to_id"].as_str()?.into(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_datasets(rows: &[Value]) -> Vec<Dataset> {
    rows.first()
        .and_then(|row| row["result"].as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|row| {
                    Some(Dataset {
                        name: row["name"].as_str()?.into(),
                        description: row["description"].as_str().unwrap_or("").into(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn normalize_datasets(datasets: &mut Vec<Dataset>, facts: &[Fact]) {
    for name in facts.iter().map(|fact| fact.dataset.as_str()) {
        if !datasets.iter().any(|dataset| dataset.name == name) {
            datasets.push(Dataset {
                name: name.into(),
                description: String::new(),
            });
        }
    }
    if !datasets
        .iter()
        .any(|dataset| dataset.name == DEFAULT_DATASET)
    {
        datasets.push(Dataset::default_dataset());
    }
    datasets.sort_by(|left, right| left.name.cmp(&right.name));
    datasets.dedup_by(|left, right| left.name == right.name);
}

fn initialize_missing_positions(facts: &mut [Fact]) -> Vec<PositionUpdate> {
    let total = facts.len().max(1);
    let mut generated = Vec::new();
    for (index, fact) in facts.iter_mut().enumerate() {
        if !fact.graph_x.is_finite() || !fact.graph_y.is_finite() {
            let angle = std::f64::consts::TAU * index as f64 / total as f64;
            let ring = 0.28 + 0.07 * ((index / 16) as f64).min(3.0);
            fact.graph_x = (0.5 + angle.cos() * ring).clamp(0.04, 0.96);
            fact.graph_y = (0.5 + angle.sin() * ring).clamp(0.04, 0.96);
            generated.push(PositionUpdate {
                index,
                x: fact.graph_x,
                y: fact.graph_y,
            });
        }
    }
    generated
}

pub fn valid_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

pub fn valid_fact_id(value: &str) -> bool {
    value.strip_prefix("fact:").is_some_and(valid_label)
}

pub fn valid_edge_id(value: &str) -> bool {
    RELATION_TYPES.iter().any(|relation| {
        value
            .strip_prefix(&format!("{relation}:"))
            .is_some_and(valid_label)
    })
}

pub fn sql_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(id: &str, x: f64, y: f64) -> Fact {
        Fact {
            id: id.into(),
            fact_type: "finding".into(),
            content: id.into(),
            source: "test".into(),
            confidence: 1.0,
            tags: Vec::new(),
            dataset: DEFAULT_DATASET.into(),
            graph_x: x,
            graph_y: y,
            graph_pinned: false,
            code_path: None,
            code_start_line: None,
            code_end_line: None,
            code_column: None,
            code_symbol: None,
            run_id: None,
            timestamp: "now".into(),
        }
    }

    #[test]
    fn filter_combines_dataset_and_type() {
        let mut first = fact("fact:a", 0.1, 0.1);
        first.dataset = "alpha".into();
        let mut second = fact("fact:b", 0.2, 0.2);
        second.dataset = "alpha".into();
        second.fact_type = "decision".into();
        let filter = GraphFilter::from_labels("alpha", "finding");
        assert_eq!(filter.indices(&[first, second]), vec![0]);
    }

    #[test]
    fn analytics_detects_duplicates_and_orphans() {
        let facts = vec![fact("fact:a", 0.1, 0.1), fact("fact:b", 0.9, 0.9)];
        let edges = vec![
            Edge {
                id: "relates_to:one".into(),
                relation_type: "relates_to".into(),
                from_id: "fact:a".into(),
                to_id: "fact:a".into(),
            },
            Edge {
                id: "relates_to:two".into(),
                relation_type: "relates_to".into(),
                from_id: "fact:a".into(),
                to_id: "fact:a".into(),
            },
        ];
        let metrics = GraphAnalytics::compute(&facts, &edges);
        assert_eq!(metrics.degrees["fact:a"], 4);
        assert_eq!(metrics.orphan_count, 1);
        assert_eq!(metrics.duplicate_edge_count, 1);
    }

    #[test]
    fn spatial_index_culls_outside_viewport() {
        let facts = vec![
            fact("fact:a", 0.49, 0.5),
            fact("fact:b", 0.9, 0.9),
            fact("fact:c", 0.52, 0.48),
        ];
        let mut index = SpatialIndex::new(16);
        index.rebuild(&facts);
        assert_eq!(
            index.query_viewport(
                Viewport {
                    center_x: 0.5,
                    center_y: 0.5,
                    zoom: 10.0,
                },
                0.0,
            ),
            vec![0, 2]
        );
    }

    #[test]
    fn layout_preserves_pinned_nodes() {
        let mut facts = vec![fact("fact:a", 0.1, 0.1), fact("fact:b", 0.9, 0.9)];
        facts[0].graph_pinned = true;
        let updates = compute_layout(&facts, &[], &[0, 1], LayoutConfig::default());
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].index, 1);
        assert_eq!((facts[0].graph_x, facts[0].graph_y), (0.1, 0.1));
    }

    #[test]
    fn record_validators_reject_surrealql_fragments() {
        assert!(valid_fact_id("fact:abc_123"));
        assert!(valid_edge_id("proves:edge-1"));
        assert!(!valid_fact_id("fact:a; DELETE fact"));
        assert!(!valid_edge_id("unknown:edge"));
    }
}
