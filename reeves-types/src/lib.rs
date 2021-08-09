use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[derive(Debug)]
pub struct FnDetail {
    pub params: Vec<String>,
    pub ret: String,
    pub s: String,
}

pub mod proto {
    use super::*;

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    #[derive(Debug)]
    pub struct SearchRequest {
        pub params: Option<Vec<String>>,
        pub ret: Option<String>,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    #[derive(Debug)]
    pub struct SearchResult {
        pub fndetails: Vec<FnDetail>,
    }
}
