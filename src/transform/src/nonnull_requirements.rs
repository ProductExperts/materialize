// Copyright Materialize, Inc. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Push non-null requirements toward sources.
//!
//! This analysis derives NonNull requirements on the arguments to predicates.
//! These requirements exist because most functions with Null arguments are
//! themselves Null, and a predicate that evaluates to Null will not pass.
//!
//! These requirements are not here introduced as constraints, but rather flow
//! to sources of data and restrict any constant collections to those rows that
//! satisfy the constraint. The main consequence is when Null values are added
//! in support of outer-joins and subqueries, we can occasionally remove that
//! branch when we observe that Null values would be subjected to predicates.
//!
//! This analysis relies on a careful understanding of `ScalarExpr` and the
//! semantics of various functions, *some of which may be non-Null even with
//! Null arguments*.
use std::collections::{HashMap, HashSet};

use crate::TransformArgs;
use expr::{Id, JoinInputMapper, RelationExpr, ScalarExpr};

/// Push non-null requirements toward sources.
#[derive(Debug)]
pub struct NonNullRequirements;

impl crate::Transform for NonNullRequirements {
    fn transform(
        &self,
        relation: &mut RelationExpr,
        _: TransformArgs,
    ) -> Result<(), crate::TransformError> {
        self.action(relation, HashSet::new(), &mut HashMap::new());
        Ok(())
    }
}

impl NonNullRequirements {
    /// Push non-null requirements toward sources.
    pub fn action(
        &self,
        relation: &mut RelationExpr,
        mut columns: HashSet<usize>,
        gets: &mut HashMap<Id, Vec<HashSet<usize>>>,
    ) {
        match relation {
            RelationExpr::Constant { rows, .. } => rows.retain(|(row, _)| {
                let datums = row.unpack();
                columns.iter().all(|c| datums[*c] != repr::Datum::Null)
            }),
            RelationExpr::Get { id, .. } => {
                gets.entry(*id).or_insert_with(Vec::new).push(columns);
            }
            RelationExpr::Let { id, value, body } => {
                // Let harvests any non-null requirements from its body,
                // and acts on the intersection of the requirements for
                // each corresponding Get, pushing them at its value.
                let id = Id::Local(*id);
                let prior = gets.insert(id, Vec::new());
                self.action(body, columns, gets);
                let mut needs = gets.remove(&id).unwrap();
                if let Some(prior) = prior {
                    gets.insert(id, prior);
                }
                if let Some(mut need) = needs.pop() {
                    while let Some(x) = needs.pop() {
                        need.retain(|col| x.contains(col))
                    }
                    self.action(value, need, gets);
                }
            }
            RelationExpr::Project { input, outputs } => {
                self.action(
                    input,
                    columns.into_iter().map(|c| outputs[c]).collect(),
                    gets,
                );
            }
            RelationExpr::Map { input, scalars } => {
                let arity = input.arity();
                if columns
                    .iter()
                    .any(|c| *c >= arity && scalars[*c - arity].is_literal_null())
                {
                    // A null value was introduced in a marked column;
                    // the entire expression can be zerod out.
                    relation.take_safely();
                } else {
                    // For each column, if it must be non-null, extract the expression's
                    // non-null requirements and include them too. We go in reverse order
                    // to ensure we squeegee down all requirements even for references to
                    // other columns produced in this operator.
                    for column in (arity..(arity + scalars.len())).rev() {
                        if columns.contains(&column) {
                            scalars[column - arity].non_null_requirements(&mut columns);
                        }
                        columns.remove(&column);
                    }
                    self.action(input, columns, gets);
                }
            }
            RelationExpr::FlatMap {
                input,
                func,
                exprs,
                demand: _,
            } => {
                if func.empty_on_null_input() {
                    for expr in exprs {
                        expr.non_null_requirements(&mut columns);
                    }
                }
                self.action(input, columns, gets);
            }
            RelationExpr::Filter { input, predicates } => {
                for predicate in predicates {
                    predicate.non_null_requirements(&mut columns);
                    // TODO: Not(IsNull) should add a constraint!
                }
                self.action(input, columns, gets);
            }
            RelationExpr::Join {
                inputs,
                equivalences,
                ..
            } => {
                let input_types = inputs.iter().map(|i| i.typ()).collect::<Vec<_>>();

                let input_mapper = JoinInputMapper::new_from_input_types(&input_types);

                let mut new_columns = input_mapper.split_column_set_by_input(&columns);

                // `variable` smears constraints around.
                // Also, any non-nullable columns impose constraints on their equivalence class.
                for equivalence in equivalences {
                    let exists_constraint = equivalence.iter().any(|expr| {
                        if let ScalarExpr::Column(c) = expr {
                            let (col, rel) = input_mapper.map_column_to_local(*c);
                            new_columns[rel].contains(&col)
                                || !input_types[rel].column_types[col].nullable
                        } else {
                            false
                        }
                    });

                    if exists_constraint {
                        for expr in equivalence.iter() {
                            if let ScalarExpr::Column(c) = expr {
                                let (col, rel) = input_mapper.map_column_to_local(*c);
                                new_columns[rel].insert(col);
                            }
                        }
                    }
                }

                for (input, columns) in inputs.iter_mut().zip(new_columns) {
                    self.action(input, columns, gets);
                }
            }
            RelationExpr::Reduce {
                input,
                group_key,
                aggregates,
                monotonic: _,
                expected_group_size: _,
            } => {
                let mut new_columns = HashSet::new();
                for column in columns {
                    // No obvious requirements on aggregate columns.
                    // A "non-empty" requirement, I guess?
                    if column < group_key.len() {
                        group_key[column].non_null_requirements(&mut new_columns);
                    }
                    if column == group_key.len() && aggregates.len() == 1 {
                        aggregates[0].expr.non_null_requirements(&mut new_columns);
                    }
                }
                self.action(input, new_columns, gets);
            }
            RelationExpr::TopK { input, .. } => {
                self.action(input, columns, gets);
            }
            RelationExpr::Negate { input } => {
                self.action(input, columns, gets);
            }
            RelationExpr::Threshold { input } => {
                self.action(input, columns, gets);
            }
            RelationExpr::Union { base, inputs } => {
                self.action(base, columns.clone(), gets);
                for input in inputs {
                    self.action(input, columns.clone(), gets);
                }
            }
            RelationExpr::ArrangeBy { input, .. } => {
                self.action(input, columns, gets);
            }
        }
    }
}
