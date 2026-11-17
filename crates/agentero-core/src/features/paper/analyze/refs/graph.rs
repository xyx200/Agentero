//! Citation relationships restored from the original References graph.

use super::{read_sidecar, CiteSidecar, SIDECAR_FILE};
use crate::error::AppError;
use crate::features::catalog::papers::{self, PaperRecord};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;

// ── Citation relationship graph ─────────────────────────────────────────────
// Built from existing `{paper}/source/agentero-cite.json` sidecars + catalog
// localMatch edges.

/// A library paper or an external reference.
#[derive(Debug, Clone, Serialize, specta::Type, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CiteGraphNodeType {
    Paper,
    Stub,
}

/// Role of a node relative to the center paper (neighborhood mode only).
#[derive(Debug, Clone, Copy, Serialize, specta::Type, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CiteGraphRole {
    /// The paper the neighborhood is built around.
    Center,
    /// Cited by the center paper.
    Reference,
    /// Library paper that cites the center paper.
    CitedBy,
}

#[derive(Debug, Clone, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct CiteGraphNode {
    /// Vault-relative paper path, or `stub:…` for unresolved citations.
    pub id: String,
    pub label: String,
    #[serde(rename = "type")]
    pub node_type: CiteGraphNodeType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Neighborhood role; absent in full-library mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<CiteGraphRole>,
}

#[derive(Debug, Clone, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct CiteGraphEdge {
    pub id: String,
    pub source: String,
    pub target: String,
    /// Citation display / raw key when available (e.g. `[12]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_raw: Option<String>,
}

#[derive(Debug, Clone, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct CiteGraphResponse {
    pub nodes: Vec<CiteGraphNode>,
    pub edges: Vec<CiteGraphEdge>,
    /// Normalized center paper path in neighborhood mode; null for full graph.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub center: Option<String>,
    pub depth: u32,
}

/// Build a citation relationship graph from catalog papers and their sidecars.
///
/// - **Full graph** (`center = None`): library-local `cites` edges only
///   (papers whose references match other catalog papers via `localMatch`).
/// - **Neighborhood** (`center = Some`): the center paper plus its direct
///   outgoing citations (library matches with `role: reference`, unmatched
///   citations as `stub` nodes) and incoming citations (library papers whose
///   sidecars match the center, `role: citedBy`). `depth` is currently
///   clamped to 1 (direct citations only).
///
/// Does not parse missing sidecars — callers that want fresh center data should
/// run `paper_refs_parse` first. Missing sidecars simply contribute no edges.
pub fn build_citation_graph(
    vault: &Path,
    center: Option<&str>,
    depth: Option<u32>,
) -> Result<CiteGraphResponse, AppError> {
    // Only direct citations are exposed, as in the original graph.
    let _ = depth;
    let depth = 1;
    let catalog = papers::list_all(vault)?;
    let by_path: HashMap<String, &PaperRecord> =
        catalog.iter().map(|r| (r.path.clone(), r)).collect();

    let mut sidecars = HashMap::new();
    for rec in &catalog {
        let path = vault.join(&rec.path).join("source").join(SIDECAR_FILE);
        if let Some(mut sidecar) = read_sidecar(&path) {
            // Imported or moved papers can match without re-parsing or rewriting sources.
            super::attach_local_matches(&mut sidecar.citations, &catalog, &rec.path);
            sidecars.insert(rec.path.clone(), sidecar);
        }
    }

    // Library-local edges: source paper path → target paper path.
    let mut lib_edges: Vec<(String, String, Option<String>)> = Vec::new();

    for rec in &catalog {
        let Some(sidecar) = sidecars.get(&rec.path) else {
            continue;
        };
        for cite in &sidecar.citations {
            let raw_hint = cite
                .display
                .clone()
                .or_else(|| cite.raw_key.clone())
                .or_else(|| cite.metadata.title.clone());
            if let Some(m) = &cite.local_match {
                let target = m
                    .paper_path
                    .trim()
                    .replace('\\', "/")
                    .trim_end_matches('/')
                    .to_string();
                if target.is_empty() || target == rec.path {
                    continue;
                }
                if by_path.contains_key(&target) {
                    lib_edges.push((rec.path.clone(), target, raw_hint));
                }
            }
        }
    }

    // Dedupe library edges by (source, target).
    lib_edges.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    lib_edges.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);

    if let Some(hint) = center {
        let center_path = resolve_center_path(&by_path, hint)
            .ok_or_else(|| AppError::message("center paper not found in catalog"))?;
        return build_neighborhood(&sidecars, &by_path, &lib_edges, &center_path, depth);
    }

    // Full graph: only library papers that participate in at least one edge.
    let mut keep_nodes: HashSet<String> = HashSet::new();
    for (s, t, _) in &lib_edges {
        keep_nodes.insert(s.clone());
        keep_nodes.insert(t.clone());
    }

    let mut nodes: Vec<CiteGraphNode> = keep_nodes
        .iter()
        .map(|id| CiteGraphNode {
            id: id.clone(),
            label: paper_label(&by_path, id),
            node_type: CiteGraphNodeType::Paper,
            path: Some(id.clone()),
            role: None,
        })
        .collect();
    nodes.sort_by(|a, b| a.id.cmp(&b.id));

    let mut edges: Vec<CiteGraphEdge> = Vec::new();
    for (i, (s, t, raw)) in lib_edges.into_iter().enumerate() {
        edges.push(CiteGraphEdge {
            id: format!("cites{i}:{s}->{t}"),
            source: s,
            target: t,
            target_raw: raw,
        });
    }
    edges.sort_by(|a, b| a.id.cmp(&b.id));

    Ok(CiteGraphResponse {
        nodes,
        edges,
        center: None,
        depth,
    })
}

/// Neighborhood around one paper: direct outgoing citations (library papers
/// plus unresolved stubs) and incoming citations from library papers.
fn build_neighborhood(
    sidecars: &HashMap<String, CiteSidecar>,
    by_path: &HashMap<String, &PaperRecord>,
    lib_edges: &[(String, String, Option<String>)],
    center: &str,
    depth: u32,
) -> Result<CiteGraphResponse, AppError> {
    // Insert-order-preserving node set; the first role wins (a mutual citation
    // keeps its outgoing "reference" role).
    let mut nodes: Vec<CiteGraphNode> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    push_node(
        CiteGraphNode {
            id: center.to_string(),
            label: paper_label(by_path, center),
            node_type: CiteGraphNodeType::Paper,
            path: Some(center.to_string()),
            role: Some(CiteGraphRole::Center),
        },
        &mut nodes,
        &mut seen,
    );

    let mut edges: Vec<(String, String, Option<String>)> = Vec::new();

    // Outgoing: the center's own references (sidecar may be absent).
    if let Some(sidecar) = sidecars.get(center) {
        for cite in &sidecar.citations {
            let raw_hint = cite
                .display
                .clone()
                .or_else(|| cite.raw_key.clone())
                .or_else(|| cite.metadata.title.clone());
            if cite
                .local_match
                .as_ref()
                .is_some_and(|m| m.paper_path.replace('\\', "/").trim_end_matches('/') == center)
            {
                continue;
            }
            let matched = cite.local_match.as_ref().and_then(|m| {
                let target = m
                    .paper_path
                    .trim()
                    .replace('\\', "/")
                    .trim_end_matches('/')
                    .to_string();
                if target.is_empty() || target == center || !by_path.contains_key(&target) {
                    None
                } else {
                    Some(target)
                }
            });
            if let Some(target) = matched {
                push_node(
                    CiteGraphNode {
                        id: target.clone(),
                        label: paper_label(by_path, &target),
                        node_type: CiteGraphNodeType::Paper,
                        path: Some(target.clone()),
                        role: Some(CiteGraphRole::Reference),
                    },
                    &mut nodes,
                    &mut seen,
                );
                edges.push((center.to_string(), target, raw_hint));
            } else {
                let id = format!("stub:{center}#{}", cite.id);
                let label = cite
                    .metadata
                    .title
                    .clone()
                    .filter(|t| !t.trim().is_empty())
                    .or_else(|| cite.raw_key.clone())
                    .unwrap_or_else(|| cite.id.clone());
                push_node(
                    CiteGraphNode {
                        id: id.clone(),
                        label,
                        node_type: CiteGraphNodeType::Stub,
                        path: None,
                        role: None,
                    },
                    &mut nodes,
                    &mut seen,
                );
                edges.push((center.to_string(), id, raw_hint));
            }
        }
    }

    // Incoming: library papers whose sidecars match the center.
    for (s, t, raw) in lib_edges {
        if t != center || s == center {
            continue;
        }
        push_node(
            CiteGraphNode {
                id: s.clone(),
                label: paper_label(by_path, s),
                node_type: CiteGraphNodeType::Paper,
                path: Some(s.clone()),
                role: Some(CiteGraphRole::CitedBy),
            },
            &mut nodes,
            &mut seen,
        );
        edges.push((s.clone(), center.to_string(), raw.clone()));
    }

    edges.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    edges.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
    let edges: Vec<CiteGraphEdge> = edges
        .into_iter()
        .enumerate()
        .map(|(i, (s, t, raw))| CiteGraphEdge {
            id: format!("cites{i}:{s}->{t}"),
            source: s,
            target: t,
            target_raw: raw,
        })
        .collect();

    nodes.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(CiteGraphResponse {
        nodes,
        edges,
        center: Some(center.to_string()),
        depth: depth.min(1),
    })
}

/// Map a center hint (paper folder or any file under it) to a catalog path.
fn resolve_center_path(by_path: &HashMap<String, &PaperRecord>, hint: &str) -> Option<String> {
    let rel = crate::fs::sanitize_vault_rel(hint).ok()?;
    let mut cur = rel.as_str();
    loop {
        if by_path.contains_key(cur) {
            return Some(cur.to_string());
        }
        let i = cur.rfind('/')?;
        cur = &cur[..i];
    }
}

/// Append a node unless its id was already seen (first role wins).
fn push_node(node: CiteGraphNode, nodes: &mut Vec<CiteGraphNode>, seen: &mut HashSet<String>) {
    if seen.insert(node.id.clone()) {
        nodes.push(node);
    }
}

fn paper_label(by_path: &HashMap<String, &PaperRecord>, id: &str) -> String {
    by_path
        .get(id)
        .map(|r| {
            let t = r.title.trim();
            if t.is_empty() {
                paper_folder_label(id)
            } else {
                t.to_string()
            }
        })
        .unwrap_or_else(|| paper_folder_label(id))
}

fn paper_folder_label(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}
