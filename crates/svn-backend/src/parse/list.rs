use super::ParseError;
use crate::model::*;
use std::io::BufRead;

pub(super) fn parse<R: BufRead>(_r: R, _emit: impl FnMut(Entry)) -> Result<(), ParseError> {
    todo!("parse::list")
}
