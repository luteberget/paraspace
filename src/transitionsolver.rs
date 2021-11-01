use std::collections::{HashMap, HashSet};

use z3::ast::{Ast, Bool, Real};

use crate::{
    from_z3_real,
    problem::{self, ObjectSet, Problem, Solution, SolutionToken, TemporalRelationship},
    SolverError,
};

// A state is a choice between several possible tokens
// in the sequence of values that make up a timeline.
struct State<'z> {
    start_time: Real<'z>,
    end_time: Real<'z>,
    timeline: usize,
    tokens: Vec<usize>,
    state_seq: usize,
}

struct Token<'a, 'z> {
    active: Option<Bool<'z>>,
    state: usize,
    value: &'a str,
    fact: bool,
}

struct Condition<'a, 'z3> {
    token_idx: usize,
    cond_spec: &'a problem::Condition,
    token_queue: usize,
    alternatives_extension: Option<Bool<'z3>>,
}

struct Timeline<'z> {
    states: Vec<usize>,
    goal_state_extension: Option<Bool<'z>>,
    facts_only: bool,
}

pub fn solve(problem: &Problem) -> Result<Solution, SolverError> {
    println!("Starting transition-and-pocl solver.");
    let z3_config = z3::Config::new();
    let ctx = z3::Context::new(&z3_config);
    let solver = z3::Solver::new(&ctx);

    let groups_by_name = problem
        .groups
        .iter()
        .map(|g| (g.name.as_str(), &g.members))
        .collect::<HashMap<_, _>>();

    let mut timelines = problem
        .timelines
        .iter()
        .map(|_| Timeline {
            states: Vec::new(),
            goal_state_extension: None,
            facts_only: false,
        })
        .collect::<Vec<_>>();

    let mut timeline_names = problem.timelines.iter().map(|t| t.name.as_str()).collect::<Vec<_>>();

    let mut states = Vec::new();
    let mut states_queue = 0;
    let mut tokens = Vec::new();
    let mut tokens_queue = 0;
    let mut conds = Vec::new();
    let mut conds_queue = 0;

    let mut goal_lits: HashMap<(&str, isize), Bool> = HashMap::new();

    let mut expand_links_queue: Vec<(bool, usize)> = Vec::new();

    let mut expand_links_lits: HashMap<Bool, usize> = HashMap::new();
    let mut expand_goal_state_lits: HashMap<Bool, usize> = HashMap::new();

    let mut timelines_by_name = problem
        .timelines
        .iter()
        .enumerate()
        .map(|(i, t)| (t.name.as_str(), i))
        .collect::<HashMap<_, _>>();

    // STATIC TOKENS

    // Add timelines for timelines that don't have a timeline specification, but still has facts (simple fact timelines).
    for const_token in problem.tokens.iter() {
        if !timelines_by_name.contains_key(const_token.timeline_name.as_str()) {
            timelines_by_name.insert(const_token.timeline_name.as_str(), timelines.len());
            timeline_names.push(const_token.timeline_name.as_str());
            timelines.push(Timeline {
                states: Vec::new(),
                goal_state_extension: None,
                facts_only: true,
            });

            assert!(timeline_names  .len() == timelines.len());
            assert!(timelines_by_name.len() == timelines.len());
        }
    }

    // The facts need to be the first states.
    for const_token in problem.tokens.iter() {
        if let crate::problem::TokenTime::Fact(start_time, end_time) = const_token.const_time {
            if !timelines[timelines_by_name[const_token.timeline_name.as_str()]]
                .states
                .is_empty()
            {
                todo!("Multiple facts.");
            }

            let token_idx = tokens.len();
            let state_idx = states.len();
            tokens.push(Token {
                active: None,
                value: &const_token.value,
                state: state_idx,
                fact: true,
            });
            states.push(State {
                state_seq: 0,
                tokens: vec![token_idx],
                start_time: start_time
                    .map(|t| Real::from_real(&ctx, t as i32, 1))
                    .unwrap_or_else(|| Real::fresh_const(&ctx, "t")),
                end_time: end_time
                    .map(|t| Real::from_real(&ctx, t as i32, 1))
                    .unwrap_or_else(|| Real::fresh_const(&ctx, "t")),
                timeline: timelines_by_name[const_token.timeline_name.as_str()],
            });
            timelines[timelines_by_name[const_token.timeline_name.as_str()]]
                .states
                .push(state_idx);
        }
    }

    // All empty timelines must now start in one of their initial states.
    for timeline in 0..timelines.len() {
        if timelines[timeline].states.is_empty() {
            assert!(timeline < problem.timelines.len());

            let expanded = expand_until(
                problem,
                &ctx,
                &solver,
                timeline,
                &mut timelines,
                &mut states,
                &mut tokens,
                None,
            );
            assert!(expanded);
        }
    }

    // REFINEMENT LOOP
    '_refinement: loop {
        // EXPAND PROBLEM FORMULATION

        while states_queue < states.len()
            || tokens_queue < tokens.len()
            || conds_queue < conds.len()
            || !expand_links_queue.is_empty()
        {
            while states_queue < states.len() {
                let state_idx = states_queue;
                states_queue += 1;

                // Does this timeline have a goal state?
                let facts_only = timelines[states[state_idx].timeline].facts_only;
                println!(
                    "Expanding state {} timeline {} (factsonly={})",
                    state_idx, states[state_idx].timeline, facts_only
                );

                // There are no goals for facts only timelines.
                if !facts_only {
                    let timeline_name = problem.timelines[states[state_idx].timeline].name.as_str();
                    if let Some(goal) = problem
                        .tokens
                        .iter()
                        .find(|const_token| const_token.timeline_name == timeline_name)
                    {
                        // Is this a potential final/goal state?
                        if let Some(token_idx) = states[state_idx]
                            .tokens
                            .iter()
                            .find(|t| tokens[**t].value == goal.value)
                        {
                            let goal_lit = Bool::fresh_const(&ctx, "goal");
                            if let Some(active) = tokens[*token_idx].active.as_ref() {
                                solver.assert(&Bool::implies(&goal_lit, active));
                            }
                            assert!(goal_lits
                                .insert((timeline_name, states[state_idx].state_seq as isize), goal_lit.clone())
                                .is_none());

                            // Select at least one goal (at most one goal is implied by the disabling of tokens below)
                            let mut clause = Vec::new();
                            if let Some(prev_extension) = timelines[timelines_by_name[timeline_name]]
                                .goal_state_extension
                                .as_ref()
                            {
                                assert!(expand_goal_state_lits.remove(prev_extension).is_some());
                                clause.push(Bool::not(prev_extension));
                            }
                            clause.push(goal_lit);

                            let extension = Bool::fresh_const(&ctx, "addgoal");
                            clause.push(extension.clone());
                            expand_goal_state_lits.insert(Bool::not(&extension), timelines_by_name[timeline_name]);
                            timelines[timelines_by_name[timeline_name]].goal_state_extension = Some(extension);

                            let clause_refs = clause.iter().collect::<Vec<_>>();
                            solver.assert(&Bool::or(&ctx, &clause_refs));
                        }
                    }

                    // Does the previous state have a goal lit?
                    if let Some(goal_in_prev_state) =
                        goal_lits.get(&(timeline_name, states[state_idx].state_seq as isize - 1))
                    {
                        for token in states[state_idx].tokens.iter().copied() {
                            if let Some(active) = tokens[token].active.as_ref() {
                                // Disable each possible token, if the previous state was a goal state.
                                solver.assert(&Bool::implies(goal_in_prev_state, &Bool::not(active)));
                            }
                        }
                    }
                }
            }

            while tokens_queue < tokens.len() {
                let token_idx = tokens_queue;
                tokens_queue += 1;

                if tokens[token_idx].fact {
                    // Minimum duration of state.
                    let prec = &Real::le(
                        &Real::add(
                            &ctx,
                            &[
                                &states[tokens[token_idx].state].start_time,
                                &Real::from_real(&ctx, 1 as i32, 1),
                            ],
                        ),
                        &states[tokens[token_idx].state].end_time,
                    );
                    solver.assert(prec);
                } else {
                    let value_spec = problem.timelines[states[tokens[token_idx].state].timeline]
                        .values
                        .iter()
                        .find(|s| s.name == tokens[token_idx].value)
                        .unwrap();

                    // Minimum duration of state.
                    let prec = &Real::le(
                        &Real::add(
                            &ctx,
                            &[
                                &states[tokens[token_idx].state].start_time,
                                &Real::from_real(&ctx, value_spec.duration.0 as i32, 1),
                            ],
                        ),
                        &states[tokens[token_idx].state].end_time,
                    );
                    if let Some(cond) = tokens[token_idx].active.as_ref() {
                        solver.assert(&Bool::implies(cond, prec))
                    } else {
                        solver.assert(prec);
                    }

                    for cond_spec in value_spec.conditions.iter() {
                        // is this a timeline transition?
                        if cond_spec
                            .is_timeline_transition(&problem.timelines[states[tokens[token_idx].state].timeline].name)
                        {
                            if states[tokens[token_idx].state].state_seq > 0 {
                                let prev_state_seq = states[tokens[token_idx].state].state_seq - 1;
                                let timeline = &timelines[states[tokens[token_idx].state].timeline];
                                let prev_state = &states[timeline.states[prev_state_seq]];

                                // find matching states
                                let matching_states = prev_state
                                    .tokens
                                    .iter()
                                    .filter_map(|t| (tokens[*t].value == cond_spec.value).then(|| &tokens[*t].active));

                                let mut clause = vec![];
                                if let Some(l) = tokens[token_idx].active.as_ref() {
                                    clause.push(Bool::not(l));
                                }

                                let mut any_const = false;
                                let mut n_lits = 0;
                                for m in matching_states {
                                    if let Some(l) = m {
                                        clause.push(l.clone());
                                        n_lits += 1;
                                    } else {
                                        any_const = true;
                                    }
                                }

                                assert!(any_const == (n_lits == 0));

                                if !any_const {
                                    let clause_refs = clause.iter().collect::<Vec<_>>();
                                    solver.assert(&Bool::or(&ctx, &clause_refs));
                                }
                            } else {
                                println!(
                                    "No transition condition for initial state for {}",
                                    &problem.timelines[states[tokens[token_idx].state].timeline].name
                                );
                            }
                        } else {
                            // When it's not a timeline transition, make a causal link.
                            conds.push(Condition {
                                token_idx,
                                token_queue: 0,
                                cond_spec,
                                alternatives_extension: None,
                            });
                        }
                    }
                }
            }

            while conds_queue < conds.len() || !expand_links_queue.is_empty() {
                let (need_new_token, cond_idx) = if conds_queue < conds.len() {
                    let cond_idx = conds_queue;
                    conds_queue += 1;
                    (true, cond_idx)
                } else {
                    expand_links_queue.pop().unwrap()
                };

                let objects: Vec<&str> = match &conds[cond_idx].cond_spec.object {
                    ObjectSet::Group(c) => groups_by_name
                        .get(c.as_str())
                        .iter()
                        .flat_map(|c| c.iter().map(String::as_str))
                        .collect::<Vec<_>>(),
                    ObjectSet::Object(n) => {
                        vec![n.as_str()]
                    }
                };

                // let mut all_target_tokens = Vec::new();
                println!("Finding tokens for object set {:?}", &conds[cond_idx].cond_spec.object);
                let mut new_target_tokens = Vec::new();
                for obj in objects.iter() {
                    println!("Finding tokens for {}.{}", obj, conds[cond_idx].cond_spec.value);
                    let timeline_idx = timelines_by_name[obj];
                    let matching_tokens = tokens.iter().enumerate().filter(|(_, t)| {
                        states[t.state].timeline == timeline_idx && t.value == conds[cond_idx].cond_spec.value
                    });
                    for (token, _) in matching_tokens {
                        // all_target_tokens.push(token);

                        if token >= conds[cond_idx].token_queue {
                            new_target_tokens.push(token);
                        }
                    }
                }

                if need_new_token && new_target_tokens.is_empty() {
                    for i in 0..objects.len() {
                        // This is a "random" (though deterministic) heuristic for which object to expand.
                        let selected_object = (tokens.len() + conds.len() + i) % objects.len();
                        let obj_name = objects[selected_object];

                        println!(
                            "Finding new states to add to get to {}.{}",
                            obj_name, conds[cond_idx].cond_spec.value
                        );

                        let prev_tokens_len = tokens.len();
                        if expand_until(
                            problem,
                            &ctx,
                            &solver,
                            timelines_by_name[obj_name],
                            &mut timelines,
                            &mut states,
                            &mut tokens,
                            Some(&conds[cond_idx].cond_spec.value),
                        ) {
                            assert!(
                                tokens[prev_tokens_len..]
                                    .iter()
                                    .filter(|t| t.value == conds[cond_idx].cond_spec.value.as_str())
                                    .count()
                                    == 1
                            );

                            new_target_tokens.push(
                                prev_tokens_len
                                    + tokens[prev_tokens_len..]
                                        .iter()
                                        .position(|t| (t.value == conds[cond_idx].cond_spec.value.as_str()))
                                        .unwrap(),
                            );

                            println!("Added token {:?}", new_target_tokens.last());
                            let token = &tokens[*new_target_tokens.last().unwrap()];
                            println!("  token state {:?} value {:?}", token.state, token.value);

                            // println!("Expanded transitions for this timeline, restarting refinement.");
                            // continue 'refinement;
                            break;
                        }
                    }
                }

                if need_new_token && new_target_tokens.is_empty() {
                    panic!("could not reach state");
                }

                let mut alternatives = Vec::new();

                if !new_target_tokens.is_empty() {
                    for token_idx in new_target_tokens.iter().copied() {
                        // Represents the usage of the causal link.
                        let choose_link = Bool::fresh_const(&ctx, "cl");

                        let temporal_rel = match conds[cond_idx].cond_spec.temporal_relationship {
                            TemporalRelationship::Meet => vec![Real::_eq(
                                &states[tokens[token_idx].state].end_time,
                                &states[tokens[conds[cond_idx].token_idx].state].start_time,
                            )],
                            TemporalRelationship::Cover => vec![
                                Real::le(
                                    &states[tokens[token_idx].state].start_time,
                                    &states[tokens[conds[cond_idx].token_idx].state].start_time,
                                ),
                                Real::le(
                                    &states[tokens[conds[cond_idx].token_idx].state].end_time,
                                    &states[tokens[token_idx].state].end_time,
                                ),
                            ],
                        };

                        if conds[cond_idx].cond_spec.amount > 0 {
                            println!("TODO Link has amount {:?}", conds[cond_idx].cond_spec);

                            // TODO

                            // // Add resource constraint for this token.
                            // let rc = resource_constraints.entry(token_idx).or_default();
                            // assert!(!rc.closed);
                            // rc.users
                            //     .push((choose_link.clone(), link.token_idx, link.linkspec.amount));
                        }

                        // The choose_link boolean implies all the condntions.
                        let mut clause = temporal_rel;
                        if let Some(active) = tokens[conds[cond_idx].token_idx].active.as_ref() {
                            clause.push(active.clone());
                        }

                        for cond in clause {
                            solver.assert(&Bool::implies(&choose_link, &cond));
                        }

                        alternatives.push(choose_link);
                    }

                    let old_expansion_lit: Option<Bool> = conds[cond_idx].alternatives_extension.take();

                    if let Some(b) = old_expansion_lit.as_ref() {
                        assert!(expand_links_lits.remove(b).is_some());
                    }

                    let expand_lit = Bool::fresh_const(&ctx, "exp");
                    assert!(expand_links_lits.insert(expand_lit.clone(), cond_idx).is_none());
                    conds[cond_idx].alternatives_extension = Some(expand_lit.clone());
                    alternatives.push(expand_lit);


                    let need_alternatives =
                        old_expansion_lit.or_else(|| tokens[conds[cond_idx].token_idx].active.clone());

                    if let Some(cond) = need_alternatives {
                        alternatives.push(Bool::not(&cond));
                    }

                    let alternatives_refs = alternatives.iter().collect::<Vec<_>>();
                    solver.assert(&Bool::or(&ctx, &alternatives_refs));
                }
                conds[cond_idx].token_queue = tokens.len();
            }

            // every time we touch something, make sure that the timeline transitions are extended all the way to a goal state.

            for const_token in problem.tokens.iter() {
                if let crate::problem::TokenTime::Goal = const_token.const_time {
                    let timeline_idx = timelines_by_name[const_token.timeline_name.as_str()];
                    let timeline = &timelines[timeline_idx];
                    let last_state = timeline.states.last().unwrap();
                    let has_goal = states[*last_state]
                        .tokens
                        .iter()
                        .any(|t| tokens[*t].value == const_token.value);
                    if !has_goal {
                        println!(
                            "Timeline {} has no final goal state. Adding.",
                            const_token.timeline_name
                        );
                        let expanded = expand_until(
                            problem,
                            &ctx,
                            &solver,
                            timeline_idx,
                            &mut timelines,
                            &mut states,
                            &mut tokens,
                            Some(const_token.value.as_str()),
                        );

                        assert!(expanded, "could not expand timeline until goal.");
                    }
                }
            }
        }

        // Now we have refined the problem enough for a potential solution to come from solving the SMT.
        // Will call the SMT solver with a list of assumptions that negate all the extension literals.
        // Extensions are:
        //  - state reaches goal and doesn't transition from then
        //  - conditions choose from the set of possible causal links
        //  - possibly: resource constraint extension literals.

        let neg_expansions = expand_links_lits
            .keys()
            .chain(expand_goal_state_lits.keys())
            .map(|l| (Bool::not(l), l.clone()))
            .collect::<HashMap<_, _>>();

        println!(
            "Solving with {} timelines {} states {} tokens {} conditions {} goal_exp {} link_exp",
            timelines.len(),
            states.len(),
            tokens.len(),
            conds.len(),
            expand_goal_state_lits.len(),
            expand_links_lits.len()
        );

        let result = solver.check_assumptions(&neg_expansions.keys().cloned().collect::<Vec<_>>());
        match result {
            z3::SatResult::Unsat => {
                let core = solver.get_unsat_core();
                if core.is_empty() {
                    return Err(SolverError::NoSolution);
                }

                // let use_trim_core = false;
                // let use_minimize_core = false;

                // if use_trim_core {
                //     crate::cores::trim_core(&mut core, &solver);
                // }

                // if use_minimize_core {
                //     crate::cores::minimize_core(&mut core, &solver);
                // }

                // core_sizes.push(core.len());
                println!("CORE SIZE #{}", core.len());
                for c in core {
                    if let Some(nc) = neg_expansions.get(&c) {
                        if let Some(timeline) = expand_goal_state_lits.get(nc) {
                            println!("Expand goals in timleine {}", problem.timelines[*timeline].name);
                            todo!()
                        } else if let Some(cond_idx) = expand_links_lits.get(&c).copied() {
                            let cond = &conds[cond_idx];
                            let token = &tokens[cond.token_idx];
                            println!(
                                "  -expand {}.{} {:?}",
                                problem.timelines[states[token.state].timeline].name, token.value, cond.cond_spec
                            );

                            // TODO heuristically decide which and how many to expand.s
                            expand_links_queue.push((true, cond_idx));
                            // need_more_links_than = links.len();
                        } else {
                            panic!("unexpected core lit");
                        }
                    } else {
                        panic!("unexpected core lit");
                    }
                }
            }

            z3::SatResult::Sat => {
                println!("SAT");
                let model = solver.get_model().unwrap();

                let mut solution_tokens = Vec::new();
                for v in tokens.iter() {
                    let active = v
                        .active
                        .as_ref()
                        .map(|a| model.eval(a, true).unwrap().as_bool().unwrap())
                        .unwrap_or(true);

                    if !active {
                        continue;
                    }

                    let start_time = from_z3_real(&model.eval(&states[v.state].start_time, true).unwrap());
                    let end_time = from_z3_real(&model.eval(&states[v.state].end_time, true).unwrap());

                    println!("value {:?}", v.value);

                    solution_tokens.push(SolutionToken {
                        object_name: timeline_names[states[v.state].timeline].to_string(),
                        value: v.value.to_string(),
                        start_time,
                        end_time,
                    })
                }

                // println!("SOLUTION {:#?}", timelines);

                return Ok(Solution {
                    tokens: solution_tokens,
                });
            }

            z3::SatResult::Unknown => {
                panic!("Z3 is undecided.")
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn expand_until<'a, 'z>(
    problem: &'a Problem,
    ctx: &'z z3::Context,
    solver: &z3::Solver,
    timeline_idx: usize,
    timelines: &mut Vec<Timeline<'z>>,
    states: &mut Vec<State<'z>>,
    tokens: &mut Vec<Token<'a, 'z>>,
    value: Option<&str>,
) -> bool {
    let n = if let Some(value) = value {
        assert!(!timelines[timeline_idx].states.is_empty());
        let prev_state = &states[*timelines[timeline_idx].states.last().unwrap()];
        let prev_values = prev_state.tokens.iter().map(|t| tokens[*t].value).collect::<Vec<_>>();

        if let Some(n) = distance_to(&problem.timelines[timeline_idx], &prev_values, value) {
            n
        } else {
            return false;
        }
    } else {
        1
    };

    assert!(n > 0);

    for _ in 0..n {
        let (state_seq, start_time, values) =
            if let Some(prev_state_idx) = timelines[timeline_idx].states.last().copied() {
                let prev_state = &states[prev_state_idx];
                let prev_values = prev_state.tokens.iter().map(|t| tokens[*t].value).collect::<Vec<_>>();
                let seq = prev_state.state_seq + 1;

                (seq, prev_state.end_time.clone(), Some(prev_values))
            } else {
                (0, Real::fresh_const(ctx, "t"), None)
            };

        let end_time = Real::fresh_const(ctx, "t");

        let state_idx = states.len();
        let token_start_idx = tokens.len();
        let values = next_values_from(&problem.timelines[timeline_idx], values.as_deref());

        let state_tokens = values
            .into_iter()
            .map(|value| Token {
                active: Some(Bool::fresh_const(ctx, "x")),
                state: state_idx,
                value,
                fact: false,
            })
            .collect::<Vec<_>>();

        if state_tokens.is_empty() {
            println!("No initial state for timeline {}", problem.timelines[timeline_idx].name);
            panic!();
        }

        // At most one state can be chosen.
        let am1 = state_tokens
            .iter()
            .filter_map(|t| t.active.as_ref().map(|b| (b, 1)))
            .collect::<Vec<_>>();
        solver.assert(&Bool::pb_le(ctx, &am1, 1));

        let token_idxs = state_tokens
            .iter()
            .enumerate()
            .map(|(i, _)| token_start_idx + i)
            .collect::<Vec<_>>();
        tokens.extend(state_tokens);
        states.push(State {
            state_seq,
            tokens: token_idxs,
            start_time,
            end_time,
            timeline: timeline_idx,
        });
        timelines[timeline_idx].states.push(state_idx);
    }

    true
}

fn next_values_from<'a>(timeline: &'a problem::Timeline, prev_values: Option<&[&'a str]>) -> HashSet<&'a str> {
    let mut next_values: HashSet<&str> = Default::default();

    for value_spec in timeline.values.iter() {
        if let Some(prev_values) = prev_values {
            // When we are looking for a next value from a previous on, if any of the
            // previous values are referred to as a transition condition, then the value is included.
            if prev_values.iter().any(|pv| {
                value_spec
                    .conditions
                    .iter()
                    .any(|c| c.is_timeline_transition_from(&timeline.name, pv))
            }) {
                next_values.insert(&value_spec.name);
            }
        } else {
            // If we are looking for an initial state, none of the conditions can be transitions conditions.
            if !value_spec
                .conditions
                .iter()
                .any(|c| c.is_timeline_transition(&timeline.name))
            {
                next_values.insert(&value_spec.name);
            }
        }
    }

    next_values
}

fn distance_to(timeline: &problem::Timeline, start_values: &[&str], goal_value: &str) -> Option<usize> {
    let mut visited_values = HashSet::new();
    let mut current_values = start_values.iter().copied().collect::<HashSet<_>>();

    let mut steps = 1;
    loop {
        let mut next_values = HashSet::new();
        for current_value in current_values.iter() {
            for value_spec in timeline.values.iter() {
                if value_spec
                    .conditions
                    .iter()
                    .any(|c| c.is_timeline_transition_from(&timeline.name, current_value))
                {
                    if goal_value == value_spec.name {
                        return Some(steps);
                    }

                    if visited_values.insert(value_spec.name.as_str()) {
                        next_values.insert(value_spec.name.as_str());
                    }
                }
            }
        }

        if next_values.is_empty() {
            return None;
        }

        current_values = next_values;
        steps += 1;
    }
}