use crate::*;
use tsrs_ast as ast;
use tsrs_core::*;
use tsrs_diagnostics as diagnostics;

impl Checker {
    // jsdoc.go:9
    pub(crate) fn check_unmatched_jsdoc_parameters(&mut self, node: P<Node>) {
        let mut jsdoc_parameters: Vec<P<Node>> = Vec::new();
        jsdoc_parameters.extend(get_all_jsdoc_tags(node).into_iter().filter(|tag| {
            if tag.kind() == Kind::JSDocParameterTag {
                let name = tag.as_jsdoc_parameter_or_property_tag().name();
                if ast::is_identifier(name) && name.text().is_empty() {
                    return false;
                }
                true
            } else {
                false
            }
        }));

        if jsdoc_parameters.is_empty() {
            return;
        }

        let is_js = ast::is_in_js_file(node);
        let mut parameters: collections::Set<String> = collections::Set::default();
        let mut excluded_parameters: collections::Set<usize> = collections::Set::default();

        for (i, &param) in node.parameters().iter().enumerate() {
            let name = param.as_parameter_declaration().name();
            if ast::is_identifier(name) {
                parameters.add(name.text().to_string());
            }
            if ast::is_binding_pattern(name) {
                excluded_parameters.add(i);
            }
        }
        if self.contains_arguments_reference(node) {
            if is_js {
                let last_jsdoc_param_index = jsdoc_parameters.len() - 1;
                let last_jsdoc_param =
                    jsdoc_parameters[last_jsdoc_param_index].as_jsdoc_parameter_or_property_tag();
                if !ast::is_identifier(last_jsdoc_param.name()) {
                    return;
                }
                if excluded_parameters.has(&last_jsdoc_param_index)
                    || parameters.has(&last_jsdoc_param.name().text().to_string())
                {
                    return;
                }
                let Some(type_expression) = last_jsdoc_param.type_expression else {
                    return;
                };
                let Some(type_node) = type_expression.type_node() else {
                    return;
                };
                let t = self.get_type_from_type_node(type_node);
                if self.is_array_type(t) {
                    return;
                }
                let name = last_jsdoc_param.name();
                self.error(
                    Some(name),
                    &diagnostics::JSDOC_PARAM_TAG_HAS_NAME_0_BUT_THERE_IS_NO_PARAMETER_WITH_THAT_NAME_IT_WOULD_MATCH_ARGUMENTS_IF_IT_HAD_AN_ARRAY_TYPE,
                    &[&name.text()],
                );
            }
        } else {
            for (index, &tag) in jsdoc_parameters.iter().enumerate() {
                let name = tag.as_jsdoc_parameter_or_property_tag().name();
                let is_name_first = tag.as_jsdoc_parameter_or_property_tag().is_name_first;

                if excluded_parameters.has(&index)
                    || (ast::is_identifier(name) && parameters.has(&name.text().to_string()))
                {
                    continue;
                }

                if ast::is_qualified_name(name) {
                    if is_js {
                        self.error(
                            Some(name),
                            &diagnostics::QUALIFIED_NAME_0_IS_NOT_ALLOWED_WITHOUT_A_LEADING_PARAM_OBJECT_1,
                            &[&crate::entity_name_to_string(name), &crate::entity_name_to_string(name.as_qualified_name().left)],
                        );
                    }
                } else if !is_name_first {
                    self.error_or_suggestion(is_js, Some(name), &diagnostics::JSDOC_PARAM_TAG_HAS_NAME_0_BUT_THERE_IS_NO_PARAMETER_WITH_THAT_NAME, &[&name.text()]);
                }
            }
        }
    }
}

// jsdoc.go:86
pub fn get_all_jsdoc_tags(node: P<Node>) -> Vec<P<Node>> {
    if !node.flags().intersects(NodeFlags::JSDoc) {
        let mut current = Some(node);
        while let Some(cur) = current {
            let jsdocs = cur.jsdoc(None);
            if !jsdocs.is_empty() {
                let last_jsdoc = jsdocs[jsdocs.len() - 1].as_jsdoc();
                if let Some(tags) = last_jsdoc.tags {
                    return tags.nodes().to_vec();
                }
            }
            current = ast::get_next_jsdoc_comment_location(cur);
        }
    }
    Vec::new()
}

impl Checker {
    // jsdoc.go:102
    pub(crate) fn check_jsdoc_augments_tag_matches_extends(
        &mut self,
        node: P<Node>,
        base_type_node: P<Node>,
        base_type: P<Type>,
    ) {
        if !ast::is_in_js_file(node) {
            return;
        }
        let file = ast::get_source_file_of_node(node);
        for &j in node.eager_jsdoc(file.map(tsrs_core::P::get)) {
            let Some(tags) = j.as_jsdoc().tags else {
                continue;
            };
            for &tag in tags.nodes() {
                if tag.kind() != Kind::JSDocAugmentsTag {
                    continue;
                }
                let source_type_node = tag.class_name();
                let source_type = self.get_type_from_type_node(source_type_node);
                let source_name = get_identifier_from_entity_name_expression(
                    source_type_node.expression().unwrap(),
                );
                let base_expression = base_type_node.expression().unwrap();
                if ast::is_entity_name_expression(base_expression) {
                    if self.is_type_identical_to(source_type, base_type) {
                        continue;
                    }
                    let target_name = get_identifier_from_entity_name_expression(base_expression);
                    if let (Some(target_name), Some(source_name)) = (target_name, source_name) {
                        self.error(
                            Some(source_name),
                            &diagnostics::JSDOC_0_1_DOES_NOT_MATCH_THE_EXTENDS_2_CLAUSE,
                            &[&tag.tag_name().text(), &source_name.text(), &target_name.text()],
                        );
                    }
                } else if let Some(source_name) = source_name
                    && !self.is_type_assignable_to(base_type, source_type)
                {
                    self.error(
                        Some(source_name),
                        &diagnostics::JSDOC_0_1_DOES_NOT_MATCH_THE_EXTENDS_2_CLAUSE,
                        &[
                            &tag.tag_name().text(),
                            &source_name.text(),
                            &tsrs_scanner::get_text_of_node(base_expression),
                        ],
                    );
                }
            }
        }
    }
}
