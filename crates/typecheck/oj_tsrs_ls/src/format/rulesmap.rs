use std::sync::OnceLock;

use tsrs_ast::Kind;

use super::*;

// rulesmap.go:11
pub(crate) fn get_rules(
    context: &FormattingContext,
    mut rules: Vec<&'static RuleImpl>,
) -> Vec<&'static RuleImpl> {
    let bucket = &get_rules_map()
        [get_rule_bucket_index(context.current_token_span.kind, context.next_token_span.kind)];
    if !bucket.is_empty() {
        let mut rule_action_mask = RuleAction::NONE;
        'outer: for &rule in bucket {
            let accept_rule_actions = !get_rule_action_exclusion(rule_action_mask);
            if rule.action() & accept_rule_actions != RuleAction::NONE {
                let preds = rule.context();
                if !preds.iter().all(|p| p(context)) {
                    continue 'outer;
                }
                rules.push(rule);
                rule_action_mask |= rule.action();
            }
        }
        return rules;
    }
    rules
}

// rulesmap.go:34
fn get_rule_bucket_index(row: Kind, column: Kind) -> usize {
    assert!(
        row <= Kind::LAST_KEYWORD && column <= Kind::LAST_KEYWORD,
        "Must compute formatting context from tokens"
    );
    (row as usize * MAP_ROW_LENGTH) + column as usize
}

// rulesmap.go:40
const MASK_BIT_SIZE: u32 = 5;
const MASK: u32 = 0b11111; // MaskBitSize bits
const MAP_ROW_LENGTH: usize = Kind::LAST_TOKEN as usize + 1;

// rulesmap.go:49
/*
 * For a given rule action, gets a mask of other rule actions that
 * cannot be applied at the same position.
 */
fn get_rule_action_exclusion(rule_action: RuleAction) -> RuleAction {
    let mut mask = RuleAction::NONE;
    if rule_action & RuleAction::STOP_PROCESSING_SPACE_ACTIONS != RuleAction::NONE {
        mask |= RuleAction::MODIFY_SPACE_ACTION;
    }
    if rule_action & RuleAction::STOP_PROCESSING_TOKEN_ACTIONS != RuleAction::NONE {
        mask |= RuleAction::MODIFY_TOKEN_ACTION;
    }
    if rule_action & RuleAction::MODIFY_SPACE_ACTION != RuleAction::NONE {
        mask |= RuleAction::MODIFY_SPACE_ACTION;
    }
    if rule_action & RuleAction::MODIFY_TOKEN_ACTION != RuleAction::NONE {
        mask |= RuleAction::MODIFY_TOKEN_ACTION;
    }
    mask
}

// rulesmap.go:66
fn get_rules_map() -> &'static Vec<Vec<&'static RuleImpl>> {
    static RULES_MAP: OnceLock<Vec<Vec<&'static RuleImpl>>> = OnceLock::new();
    RULES_MAP.get_or_init(build_rules_map)
}

// rulesmap.go:68
fn build_rules_map() -> Vec<Vec<&'static RuleImpl>> {
    let rules = get_all_rules();
    // Map from bucket index to array of rules
    let mut m: Vec<Vec<&'static RuleImpl>> = vec![Vec::new(); MAP_ROW_LENGTH * MAP_ROW_LENGTH];
    // This array is used only during construction of the rulesbucket in the map
    let mut rules_bucket_construction_state_list: Vec<u32> = vec![0; m.len()];
    for rule in &rules {
        let specific_rule = rule.left_token_range.is_specific && rule.right_token_range.is_specific;

        for &left in &rule.left_token_range.tokens {
            for &right in &rule.right_token_range.tokens {
                let index = get_rule_bucket_index(left, right);
                add_rule(
                    &mut m[index],
                    rule.rule,
                    specific_rule,
                    &mut rules_bucket_construction_state_list,
                    index,
                );
            }
        }
    }
    m
}

// rulesmap.go:87
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RulesPosition(u32);

impl RulesPosition {
    const STOP_RULES_SPECIFIC: RulesPosition = RulesPosition(0);
    const STOP_RULES_ANY: RulesPosition = RulesPosition(MASK_BIT_SIZE);
    const CONTEXT_RULES_SPECIFIC: RulesPosition = RulesPosition(MASK_BIT_SIZE * 2);
    const CONTEXT_RULES_ANY: RulesPosition = RulesPosition(MASK_BIT_SIZE * 3);
    const NO_CONTEXT_RULES_SPECIFIC: RulesPosition = RulesPosition(MASK_BIT_SIZE * 4);
    const NO_CONTEXT_RULES_ANY: RulesPosition = RulesPosition(MASK_BIT_SIZE * 5);
}

// rulesmap.go:114
// The Rules list contains all the inserted rules into a rulebucket in the following order:
//
//	1- Ignore rules with specific token combination
//	2- Ignore rules with any token combination
//	3- Context rules with specific token combination
//	4- Context rules with any token combination
//	5- Non-context rules with specific token combination
//	6- Non-context rules with any token combination
//
// The member rulesInsertionIndexBitmap is used to describe the number of rules
// in each sub-bucket (above) hence can be used to know the index of where to insert
// the next rule. It's a bitmap which contains 6 different sections each is given 5 bits.
//
// Example:
// In order to insert a rule to the end of sub-bucket (3), we get the index by adding
// the values in the bitmap segments 3rd, 2nd, and 1st.
fn add_rule(
    rules: &mut Vec<&'static RuleImpl>,
    rule: &'static RuleImpl,
    specific_tokens: bool,
    construction_state: &mut [u32],
    rules_bucket_index: usize,
) {
    let position = if rule.action() & RuleAction::STOP_ACTION != RuleAction::NONE {
        if specific_tokens {
            RulesPosition::STOP_RULES_SPECIFIC
        } else {
            RulesPosition::STOP_RULES_ANY
        }
    } else if !rule.context().is_empty() {
        if specific_tokens {
            RulesPosition::CONTEXT_RULES_SPECIFIC
        } else {
            RulesPosition::CONTEXT_RULES_ANY
        }
    } else if specific_tokens {
        RulesPosition::NO_CONTEXT_RULES_SPECIFIC
    } else {
        RulesPosition::NO_CONTEXT_RULES_ANY
    };

    let state = construction_state[rules_bucket_index];

    rules.insert(get_rule_insertion_index(state, position), rule);
    construction_state[rules_bucket_index] = increase_insertion_index(state, position);
}

// rulesmap.go:143
fn get_rule_insertion_index(mut index_bitmap: u32, mask_position: RulesPosition) -> usize {
    let mut index: u32 = 0;
    let mut pos = 0;
    while pos <= mask_position.0 {
        index += index_bitmap & MASK;
        index_bitmap >>= MASK_BIT_SIZE;
        pos += MASK_BIT_SIZE;
    }
    index as usize
}

// rulesmap.go:152
fn increase_insertion_index(index_bitmap: u32, mask_position: RulesPosition) -> u32 {
    let value = ((index_bitmap >> mask_position.0) & MASK) + 1;
    assert!(
        (value & MASK) == value,
        "Adding more rules into the sub-bucket than allowed. Maximum allowed is 32 rules."
    );
    (index_bitmap & !(MASK << mask_position.0)) | (value << mask_position.0)
}
