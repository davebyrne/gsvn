use super::ParseError;
use crate::model::*;
use std::io::BufRead;

pub(super) fn parse<R: BufRead>(_r: R) -> Result<Vec<Property>, ParseError> {
    todo!("parse::proplist")
}
