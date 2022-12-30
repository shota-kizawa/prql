use anyhow::Result;
use itertools::Itertools;
use std::collections::{HashMap, HashSet};

use crate::ast::rq::{
    self, fold_transform, CId, Compute, Expr, Relation, RelationColumn, RelationKind, RqFold,
    TableDecl, TableRef, Transform,
};

use super::context::{AnchorContext, ColumnDecl};

type RemainingPipeline = (Vec<Transform>, Vec<CId>);

/// Splits pipeline into two parts, such that the second part contains
/// maximum number of transforms while "fitting" into a SELECT query.
pub fn split_off_back(
    ctx: &mut AnchorContext,
    output: Vec<CId>,
    mut pipeline: Vec<Transform>,
) -> (Option<RemainingPipeline>, Vec<Transform>) {
    if pipeline.is_empty() {
        return (None, Vec::new());
    }

    log::debug!("traversing pipeline to obtain columns: {output:?}");

    let mut following_transforms: HashSet<String> = HashSet::new();

    let mut inputs_required = into_requirements(output.clone(), Complexity::highest(), true);
    let mut inputs_avail = HashSet::new();

    // iterate backwards
    let mut curr_pipeline_rev = Vec::new();
    'pipeline: while let Some(transform) = pipeline.pop() {
        // stop if split is needed
        let split = is_split_required(&transform, &mut following_transforms);
        if split {
            log::debug!("split required after {}", transform.as_ref());
            log::debug!(".. following={:?}", following_transforms);
            pipeline.push(transform);
            break;
        }

        // anchor and record all requirements
        let required = get_requirements(&transform, &following_transforms);
        log::debug!("transform {} requires {:?}", transform.as_ref(), required);
        inputs_required.extend(required);

        match &transform {
            Transform::Compute(compute) => {
                if can_materialize(compute, &inputs_required) {
                    log::debug!("materializing {:?}", compute.id);
                    inputs_avail.insert(compute.id);
                } else {
                    pipeline.push(transform);
                    break;
                }
            }
            Transform::Aggregate { compute, .. } => {
                for cid in compute {
                    let decl = &ctx.column_decls[cid];
                    if let ColumnDecl::Compute(compute) = decl {
                        if !can_materialize(compute, &inputs_required) {
                            pipeline.push(transform);
                            break 'pipeline;
                        }
                    }
                }
            }
            Transform::From(with) | Transform::Join { with, .. } => {
                for (_, cid) in &with.columns {
                    inputs_avail.insert(*cid);
                }
            }
            _ => (),
        }

        // push into current pipeline
        if !matches!(transform, Transform::Select(_)) {
            curr_pipeline_rev.push(transform);
        }
    }

    let selected = inputs_required
        .iter()
        .filter(|r| r.selected)
        .map(|r| r.col)
        .collect_vec();

    log::debug!("finished table:");
    log::debug!(".. avail={inputs_avail:?}");

    let required = inputs_required
        .into_iter()
        .map(|r| r.col)
        .unique()
        .collect_vec();
    log::debug!(".. required={required:?}");

    let missing = required
        .into_iter()
        .filter(|i| !inputs_avail.contains(i))
        .collect_vec();
    log::debug!(".. missing={missing:?}");

    // figure out SELECT columns
    {
        // output cols must preserve duplicates, but selected inputs has to be deduplicated
        let mut output = output;
        for c in selected {
            if !output.contains(&c) {
                output.push(c);
            }
        }

        // Because of s-strings, sometimes, transforms will not have any
        // requirements, which would result in empty SELECTs.
        // As a workaround, let's just fallback to a wildcard.
        let output = if output.is_empty() {
            let (input_tables, _) = ctx.collect_pipeline_inputs(&pipeline);

            input_tables
                .iter()
                .map(|tiid| ctx.register_wildcard(*tiid))
                .collect()
        } else {
            output
        };

        curr_pipeline_rev.push(Transform::Select(output));
    }

    let remaining_pipeline = if pipeline.is_empty() {
        None
    } else {
        // drop inputs that were satisfied in current pipeline

        Some((pipeline, missing))
    };

    curr_pipeline_rev.reverse();
    (remaining_pipeline, curr_pipeline_rev)
}

fn can_materialize(compute: &Compute, inputs_required: &[Requirement]) -> bool {
    let complexity = infer_complexity(compute);

    let required_max = inputs_required
        .iter()
        .filter(|r| r.col == compute.id)
        .fold(Complexity::highest(), |c, r| {
            Complexity::min(c, r.max_complexity)
        });

    let can = complexity <= required_max;
    if !can {
        log::debug!(
            "{:?} has complexity {complexity:?}, but is required to have max={required_max:?}",
            compute.id
        );
    }
    can
}

/// Applies adjustments to second part of a pipeline when it's split:
/// - prepend pipeline with From
/// - redefine columns materialized in preceding pipeline
/// - redirect all references to original columns to the new ones
pub fn anchor_split(
    ctx: &mut AnchorContext,
    first_table_name: &str,
    cols_at_split: &[CId],
    second_pipeline: Vec<Transform>,
) -> Vec<Transform> {
    let new_tid = ctx.tid.gen();

    log::debug!("split pipeline, first pipeline output: {cols_at_split:?}");

    // define columns of the new CTE
    let mut cid_redirects = HashMap::<CId, CId>::new();
    let mut new_columns = Vec::new();
    for old_cid in cols_at_split {
        let new_cid = ctx.cid.gen();

        let old_name = ctx.ensure_column_name(*old_cid).cloned();
        if let Some(name) = old_name.clone() {
            ctx.column_names.insert(new_cid, name);
        }

        let old_def = ctx.column_decls.get(old_cid).unwrap();

        let col = match old_def {
            ColumnDecl::RelationColumn(_, _, RelationColumn::Wildcard) => RelationColumn::Wildcard,
            _ => RelationColumn::Single(old_name),
        };

        new_columns.push((col, new_cid));
        cid_redirects.insert(*old_cid, new_cid);
    }

    // define a new table
    ctx.table_decls.insert(
        new_tid,
        TableDecl {
            id: new_tid,
            name: Some(first_table_name.to_string()),
            // here we should put the pipeline, but because how this function is called,
            // we need to return the pipeline directly, so we just insert dummy expr instead
            relation: Relation {
                kind: RelationKind::SString(vec![]),
                columns: vec![],
            },
        },
    );

    // define instance of that table
    let table_ref = TableRef {
        source: new_tid,
        name: Some(first_table_name.to_string()),
        columns: new_columns,
    };
    ctx.create_table_instance(table_ref.clone());

    // adjust second part: prepend from and rewrite expressions to use new columns
    let mut second = second_pipeline;
    second.insert(0, Transform::From(table_ref));

    let mut redirector = CidRedirector { ctx, cid_redirects };
    redirector.fold_transforms(second).unwrap()
}

/// Determines whether a pipeline must be split at a transform to
/// fit into one SELECT statement.
///
/// `following` contain names of following transforms in the pipeline.
fn is_split_required(transform: &Transform, following: &mut HashSet<String>) -> bool {
    // Pipeline must be split when there is a transform that is out of order:
    // - from (max 1x),
    // - join (no limit),
    // - filters (for WHERE)
    // - aggregate (max 1x)
    // - filters (for HAVING)
    // - compute (no limit)
    // - sort (no limit)
    // - take (no limit)
    // - unique (for DISTINCT)
    // - concat (max 1)
    // - unique (for UNION)
    //
    // Select is not affected by the order.
    use Transform::*;

    // Compute for aggregation does not count as a real compute,
    // because it's done within the aggregation
    if let Compute(decl) = transform {
        if decl.is_aggregation {
            return false;
        }
    }

    fn contains_any<const C: usize>(set: &HashSet<String>, elements: [&'static str; C]) -> bool {
        for t in elements {
            if set.contains(t) {
                return true;
            }
        }
        false
    }

    let split = match transform {
        From(_) => contains_any(following, ["From"]),
        Join { .. } => contains_any(following, ["From"]),
        Aggregate { .. } => contains_any(following, ["From", "Join", "Aggregate"]),
        Filter(_) => contains_any(following, ["From", "Join"]),
        Compute(_) => contains_any(following, ["From", "Join", /* "Aggregate" */ "Filter"]),
        Sort(_) => contains_any(following, ["From", "Join", "Compute", "Aggregate"]),
        Take(_) => contains_any(
            following,
            ["From", "Join", "Compute", "Filter", "Aggregate", "Sort"],
        ),
        Unique => contains_any(
            following,
            [
                "From",
                "Join",
                "Compute",
                "Filter",
                "Aggregate",
                "Sort",
                "Take",
            ],
        ),
        Concat(_) => contains_any(
            following,
            [
                "From",
                "Join",
                "Compute",
                "Filter",
                "Aggregate",
                "Sort",
                "Take",
                "Concat",
            ],
        ),
        _ => false,
    };

    if !split {
        following.insert(transform.as_ref().to_string());
    }
    split
}

/// An input requirement of a transform.
pub struct Requirement {
    pub col: CId,

    /// Maxium complexity with which this column can be expressed in this transform
    pub max_complexity: Complexity,

    /// True iff this column needs to be SELECTed so I can be referenced in this transform
    pub selected: bool,
}

fn into_requirements(
    cids: Vec<CId>,
    max_complexity: Complexity,
    selected: bool,
) -> Vec<Requirement> {
    cids.into_iter()
        .map(|col| Requirement {
            col,
            max_complexity,
            selected,
        })
        .collect()
}

impl std::fmt::Debug for Requirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.col, f)?;
        f.write_str("-as-")?;
        std::fmt::Debug::fmt(&self.max_complexity, f)
    }
}

pub fn get_requirements(transform: &Transform, following: &HashSet<String>) -> Vec<Requirement> {
    use Transform::*;

    if let Aggregate { partition, compute } = transform {
        let mut r = Vec::new();
        r.extend(into_requirements(
            partition.clone(),
            Complexity::Plain,
            false,
        ));
        r.extend(into_requirements(
            compute.clone(),
            Complexity::Aggregation,
            false,
        ));
        return r;
    }

    let cids = match transform {
        Compute(compute) => CidCollector::collect(compute.expr.clone()),
        Filter(expr) | Join { filter: expr, .. } => CidCollector::collect(expr.clone()),
        Sort(sorts) => sorts.iter().map(|s| s.column).collect(),
        Take(rq::Take { range, .. }) => {
            let mut cids = Vec::new();
            if let Some(e) = &range.start {
                cids.extend(CidCollector::collect(e.clone()));
            }
            if let Some(e) = &range.end {
                cids.extend(CidCollector::collect(e.clone()));
            }
            cids
        }

        Select(_) | From(_) | Concat(_) | Aggregate { .. } | Unique => return Vec::new(),
    };

    let (max_complexity, selected) = match transform {
        Compute(decl) => (
            if infer_complexity(decl) == Complexity::Plain {
                Complexity::Aggregation
            } else {
                Complexity::Plain
            },
            false,
        ),
        Filter(_) => (
            if !following.contains("Aggregate") {
                Complexity::Aggregation
            } else {
                Complexity::Plain
            },
            false,
        ),
        // ORDER BY uses aliased columns, so the columns can have high complexity
        Sort(_) => (Complexity::Aggregation, true),
        Take(_) => (Complexity::Plain, false),
        Join { .. } => (Complexity::Plain, false),

        _ => unreachable!(),
    };

    into_requirements(cids, max_complexity, selected)
}

/// Complexity of a column expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Complexity {
    /// Non-aggregated and non-windowed expressions
    Plain,
    /// Non-aggregated expressions
    Windowed,
    /// Everything
    Aggregation,
}

impl Complexity {
    const fn highest() -> Self {
        Self::Aggregation
    }
}

pub fn infer_complexity(compute: &Compute) -> Complexity {
    use Complexity::*;

    if compute.window.is_some() {
        Windowed
    } else if compute.is_aggregation {
        Aggregation
    } else {
        Plain
    }
}

#[derive(Default)]
pub struct CidCollector {
    cids: HashSet<CId>,
}

impl CidCollector {
    pub fn collect(expr: Expr) -> Vec<CId> {
        let mut collector = CidCollector::default();
        collector.fold_expr(expr).unwrap();
        collector.cids.into_iter().collect_vec()
    }
}

impl RqFold for CidCollector {
    fn fold_cid(&mut self, cid: CId) -> Result<CId> {
        self.cids.insert(cid);
        Ok(cid)
    }
}

struct CidRedirector<'a> {
    ctx: &'a mut AnchorContext,
    cid_redirects: HashMap<CId, CId>,
}

impl<'a> RqFold for CidRedirector<'a> {
    fn fold_cid(&mut self, cid: CId) -> Result<CId> {
        Ok(self.cid_redirects.get(&cid).cloned().unwrap_or(cid))
    }

    fn fold_transform(&mut self, transform: Transform) -> Result<Transform> {
        match transform {
            Transform::Compute(compute) => {
                let compute = self.fold_compute(compute)?;
                self.ctx.register_compute(compute.clone());
                Ok(Transform::Compute(compute))
            }
            _ => fold_transform(self, transform),
        }
    }
}