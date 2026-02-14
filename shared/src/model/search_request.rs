use std::rc::Rc;

#[derive(Debug, Clone, PartialEq)]
pub enum SearchRequest {
    Clear,
    Text(String, Option<Rc<Vec<String>>>),
    Regexp(String, Option<Rc<Vec<String>>>),
}
