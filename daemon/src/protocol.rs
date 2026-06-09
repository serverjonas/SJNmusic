use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub enum Command {
    Play(String),
    Add(String),
    Del(String),
    Init(String),
    Search(String),
}
