//! Non-function declarations of `jsx.go`.

use std::rc::Rc;

use crate::direct_bitflags as bitflags;

use crate::*;

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    pub struct JsxFlags: u32 {
        const None = 0;
        const IntrinsicNamedElement = 1 << 0; // An element from a named property of the JSX.IntrinsicElements interface
        const IntrinsicIndexedElement = 1 << 1; // An element inferred from the string index signature of the JSX.IntrinsicElements interface
        const IntrinsicElement = Self::IntrinsicNamedElement.bits() | Self::IntrinsicIndexedElement.bits();
    }
}

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum JsxReferenceKind {
    #[default]
    Component,
    Function,
    Mixed,
}

#[derive(Default)]
pub struct JsxElementLinks {
    pub(crate) jsx_flags: Cell<JsxFlags>, // Flags for the JSX element
    pub(crate) resolved_jsx_element_attributes_type: Cell<Option<P<Type>>>, // Resolved element attributes type of a JSX opening-like element
    pub(crate) jsx_namespace: Cell<Option<P<Symbol>>>, // Resolved JSX namespace symbol for this node
    pub(crate) jsx_implicit_import_container: Cell<Option<P<Symbol>>>, // Resolved module symbol the implicit JSX import of this file should refer to
    pub(crate) first_jsx_tag_in_file: Cell<Option<P<Node>>>, // The first JSX tag in the file
}

pub(crate) struct JsxNamesStruct {
    pub(crate) jsx: &'static str,
    pub(crate) intrinsic_elements: &'static str,
    pub(crate) element_class: &'static str,
    pub(crate) element_attributes_property_name_container: &'static str,
    pub(crate) element_children_attribute_name_container: &'static str,
    pub(crate) element: &'static str,
    pub(crate) element_type: &'static str,
    pub(crate) intrinsic_attributes: &'static str,
    pub(crate) intrinsic_class_attributes: &'static str,
    pub(crate) library_managed_attributes: &'static str,
}

pub(crate) static JSX_NAMES: JsxNamesStruct = JsxNamesStruct {
    jsx: "JSX",
    intrinsic_elements: "IntrinsicElements",
    element_class: "ElementClass",
    element_attributes_property_name_container: "ElementAttributesProperty",
    element_children_attribute_name_container: "ElementChildrenAttribute",
    element: "Element",
    element_type: "ElementType",
    intrinsic_attributes: "IntrinsicAttributes",
    intrinsic_class_attributes: "IntrinsicClassAttributes",
    library_managed_attributes: "LibraryManagedAttributes",
};

pub(crate) struct ReactNamesStruct {
    pub(crate) fragment: &'static str,
}

pub(crate) static REACT_NAMES: ReactNamesStruct = ReactNamesStruct { fragment: "Fragment" };

#[derive(Clone, Default)]
pub struct JsxElaborationElement {
    pub(crate) error_node: Option<P<Node>>,
    pub(crate) inner_expression: Option<P<Node>>,
    pub(crate) name_type: Option<P<Type>>,
    pub(crate) create_diagnostic: Option<Rc<dyn Fn(&mut Checker, P<Node>) -> P<Diagnostic>>>, // Optional: creates a custom diagnostic for this element
}
