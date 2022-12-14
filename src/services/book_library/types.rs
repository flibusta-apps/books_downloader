use serde::Deserialize;


#[derive(Deserialize, Debug, Clone)]
pub struct Source {
    // id: u32,
    // name: String
}

#[derive(Deserialize, Debug, Clone)]
pub struct BookAuthor {
    pub id: u32,
    pub first_name: String,
    pub last_name: String,
    pub middle_name: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Book {
    pub id: u32,
    pub remote_id: u32,
    pub title: String,
    pub lang: String,
    pub file_type: String,
    pub uploaded: String,
    pub authors: Vec<BookAuthor>,
}
