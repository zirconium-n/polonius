// Copyright 2017 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! A version of the Naive datalog analysis using Datafrog.

use datafrog::{Iteration, Relation, RelationLeaper};
use std::time::Instant;

use crate::facts::FactTypes;
use crate::output::{Context, Output};

pub(super) fn compute<T: FactTypes>(
    ctx: &Context<T>,
    result: &mut Output<T>,
) -> Relation<(T::Loan, T::Point)> {
    let timer = Instant::now();

    let errors = {
        let all_facts = &ctx.all_facts;

        // Static inputs
        let region_live_at_rel = &ctx.region_live_at;
        let cfg_edge_rel = &ctx.cfg_edge;
        let killed_rel = &ctx.killed;

        // Create a new iteration context, ...
        let mut iteration = Iteration::new();

        // .. some variables, ..
        let subset = iteration.variable::<(T::Origin, T::Origin, T::Point)>("subset");
        let requires = iteration.variable::<(T::Origin, T::Loan, T::Point)>("requires");
        let borrow_live_at = iteration.variable::<((T::Loan, T::Point), ())>("borrow_live_at");

        // `invalidates` facts, stored ready for joins
        let invalidates = iteration.variable::<((T::Loan, T::Point), ())>("invalidates");

        // different indices for `subset`.
        let subset_o1p = iteration.variable_indistinct("subset_o1p");
        let subset_o2p = iteration.variable_indistinct("subset_o2p");

        // different index for `requires`.
        let requires_op = iteration.variable_indistinct("requires_op");

        // we need `region_live_at` in both variable and relation forms.
        // (respectively, for the regular join and the leapjoin).
        let region_live_at_var =
            iteration.variable::<((T::Origin, T::Point), ())>("region_live_at");

        // output
        let errors = iteration.variable("errors");

        // load initial facts.
        subset.extend(all_facts.outlives.iter());
        requires.extend(all_facts.borrow_region.iter());
        invalidates.extend(
            all_facts
                .invalidates
                .iter()
                .map(|&(point, loan)| ((loan, point), ())),
        );
        region_live_at_var.extend(
            region_live_at_rel
                .iter()
                .map(|&(origin, point)| ((origin, point), ())),
        );

        // .. and then start iterating rules!
        while iteration.changed() {
            // Cleanup step: remove symmetries
            // - remove regions which are `subset`s of themselves
            //
            // FIXME: investigate whether is there a better way to do that without complicating
            // the rules too much, because it would also require temporary variables and
            // impact performance. Until then, the big reduction in tuples improves performance
            // a lot, even if we're potentially adding a small number of tuples
            // per round just to remove them in the next round.
            subset
                .recent
                .borrow_mut()
                .elements
                .retain(|&(origin1, origin2, _)| origin1 != origin2);

            // remap fields to re-index by keys.
            subset_o1p.from_map(&subset, |&(origin1, origin2, point)| {
                ((origin1, point), origin2)
            });
            subset_o2p.from_map(&subset, |&(origin1, origin2, point)| {
                ((origin2, point), origin1)
            });

            requires_op.from_map(&requires, |&(origin, loan, point)| ((origin, point), loan));

            // subset(origin1, origin2, point) :- outlives(origin1, origin2, point).
            // Already loaded; outlives is static.

            // subset(origin1, origin3, point) :-
            //   subset(origin1, origin2, point),
            //   subset(origin2, origin3, point).
            subset.from_join(
                &subset_o2p,
                &subset_o1p,
                |&(_origin2, point), &origin1, &origin3| (origin1, origin3, point),
            );

            // subset(origin1, origin2, point2) :-
            //   subset(origin1, origin2, point1),
            //   cfg_edge(point1, point2),
            //   region_live_at(origin1, point2),
            //   region_live_at(origin2, point2).
            subset.from_leapjoin(
                &subset,
                (
                    cfg_edge_rel.extend_with(|&(_origin1, _origin2, point1)| point1),
                    region_live_at_rel.extend_with(|&(origin1, _origin2, _point1)| origin1),
                    region_live_at_rel.extend_with(|&(_origin1, origin2, _point1)| origin2),
                ),
                |&(origin1, origin2, _point1), &point2| (origin1, origin2, point2),
            );

            // requires(origin, loan, point) :- borrow_region(origin, loan, point).
            // Already loaded; borrow_region is static.

            // requires(origin2, loan, point) :-
            //   requires(origin1, loan, point),
            //   subset(origin1, origin2, point).
            requires.from_join(
                &requires_op,
                &subset_o1p,
                |&(_origin1, point), &loan, &origin2| (origin2, loan, point),
            );

            // requires(origin, loan, point2) :-
            //   requires(origin, loan, point1),
            //   !killed(loan, point1),
            //   cfg_edge(point1, point2),
            //   region_live_at(origin, point2).
            requires.from_leapjoin(
                &requires,
                (
                    killed_rel.filter_anti(|&(_origin, loan, point1)| (loan, point1)),
                    cfg_edge_rel.extend_with(|&(_origin, _loan, point1)| point1),
                    region_live_at_rel.extend_with(|&(origin, _loan, _point1)| origin),
                ),
                |&(origin, loan, _point1), &point2| (origin, loan, point2),
            );

            // borrow_live_at(loan, point) :-
            //   requires(origin, loan, point),
            //   region_live_at(origin, point).
            borrow_live_at.from_join(
                &requires_op,
                &region_live_at_var,
                |&(_origin, point), &loan, _| ((loan, point), ()),
            );

            // errors(loan, point) :-
            //   invalidates(loan, point),
            //   borrow_live_at(loan, point).
            errors.from_join(&invalidates, &borrow_live_at, |&(loan, point), _, _| {
                (loan, point)
            });
        }

        if result.dump_enabled {
            let subset = subset.complete();
            assert!(
                subset
                    .iter()
                    .filter(|&(origin1, origin2, _)| origin1 == origin2)
                    .count()
                    == 0,
                "unwanted subset symmetries"
            );
            for &(origin1, origin2, location) in subset.iter() {
                result
                    .subset
                    .entry(location)
                    .or_default()
                    .entry(origin1)
                    .or_default()
                    .insert(origin2);
            }

            let requires = requires.complete();
            for &(origin, loan, location) in requires.iter() {
                result
                    .restricts
                    .entry(location)
                    .or_default()
                    .entry(origin)
                    .or_default()
                    .insert(loan);
            }

            let borrow_live_at = borrow_live_at.complete();
            for &((loan, location), _) in borrow_live_at.iter() {
                result
                    .borrow_live_at
                    .entry(location)
                    .or_default()
                    .push(loan);
            }
        }

        errors.complete()
    };

    if result.dump_enabled {
        info!(
            "errors is complete: {} tuples, {:?}",
            errors.len(),
            timer.elapsed()
        );
    }

    errors
}
