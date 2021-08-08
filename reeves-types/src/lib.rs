use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[derive(Debug)]
pub struct FnDetail {
    pub params: String,
    pub ret: String,
    pub s: String,
}

pub mod proto {
    use super::*;

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    #[derive(Debug)]
    pub struct SearchRequest {
        pub params: String,
        pub ret: String,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    #[derive(Debug)]
    pub struct SearchResult {
        pub fndetails: Vec<FnDetail>,
    }
}
