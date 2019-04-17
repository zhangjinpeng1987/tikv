// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use tipb::expression::FieldType;

use super::super::function::RpnFunction;
use crate::coprocessor::codec::data_type::ScalarValue;

/// A type for each node in the RPN expression list.
#[derive(Debug)]
pub enum RpnExpressionNode {
    /// Represents a function call.
    FnCall {
        func: Box<dyn RpnFunction>,
        field_type: FieldType,
    },

    /// Represents a scalar constant value.
    Constant {
        value: ScalarValue,
        field_type: FieldType,
    },

    /// Represents a reference to a column in the columns specified in evaluation.
    ColumnRef { offset: usize },
}

impl RpnExpressionNode {
    /// Gets the field type.
    #[inline]
    pub fn field_type(&self) -> Option<&FieldType> {
        match self {
            RpnExpressionNode::FnCall { ref field_type, .. } => Some(field_type),
            RpnExpressionNode::Constant { ref field_type, .. } => Some(field_type),
            RpnExpressionNode::ColumnRef { .. } => None,
        }
    }

    /// Borrows the function instance for `FnCall` variant.
    #[inline]
    pub fn fn_call_func(&self) -> Option<&dyn RpnFunction> {
        match self {
            RpnExpressionNode::FnCall { ref func, .. } => Some(&*func),
            _ => None,
        }
    }

    /// Borrows the constant value for `Constant` variant.
    #[inline]
    pub fn constant_value(&self) -> Option<&ScalarValue> {
        match self {
            RpnExpressionNode::Constant { ref value, .. } => Some(value),
            _ => None,
        }
    }

    /// Gets the column offset for `ColumnRef` variant.
    #[inline]
    pub fn column_ref_offset(&self) -> Option<usize> {
        match self {
            RpnExpressionNode::ColumnRef { ref offset, .. } => Some(*offset),
            _ => None,
        }
    }
}

/// An expression in Reverse Polish notation, which is simply a list of RPN expression nodes.
///
/// You may want to build it using `RpnExpressionBuilder`.
#[derive(Debug)]
pub struct RpnExpression(Vec<RpnExpressionNode>);

impl std::ops::Deref for RpnExpression {
    type Target = Vec<RpnExpressionNode>;

    fn deref(&self) -> &Vec<RpnExpressionNode> {
        &self.0
    }
}

impl std::ops::DerefMut for RpnExpression {
    fn deref_mut(&mut self) -> &mut Vec<RpnExpressionNode> {
        &mut self.0
    }
}

impl From<Vec<RpnExpressionNode>> for RpnExpression {
    fn from(v: Vec<RpnExpressionNode>) -> Self {
        Self(v)
    }
}

impl AsRef<[RpnExpressionNode]> for RpnExpression {
    fn as_ref(&self) -> &[RpnExpressionNode] {
        self.0.as_ref()
    }
}

impl RpnExpression {
    /// Gets the field type of the return value.
    pub fn ret_field_type<'a>(&'a self, schema: &'a [FieldType]) -> &'a FieldType {
        assert!(!self.0.is_empty());
        let last_node = self.0.last().unwrap();
        match last_node {
            RpnExpressionNode::FnCall { field_type, .. } => field_type,
            RpnExpressionNode::Constant { field_type, .. } => field_type,
            RpnExpressionNode::ColumnRef { offset } => &schema[*offset],
        }
    }
}

// For `RpnExpression::eval`, see `expr_eval` file.
