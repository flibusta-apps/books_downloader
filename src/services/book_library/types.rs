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
    pub title: String,
    pub lang: String,
    pub file_type: String,
    pub uploaded: String,
    pub authors: Vec<BookAuthor>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct BookWithRemote {
    pub id: u32,
    pub remote_id: u32,
    pub title: String,
    pub lang: String,
    pub file_type: String,
    pub uploaded: String,
    pub authors: Vec<BookAuthor>,
}

impl BookWithRemote {
    pub fn from_book(book: Book, remote_id: u32) -> Self {
        Self {
            id: book.id,
            remote_id,
            title: book.title,
            lang: book.lang,
            file_type: book.file_type,
            uploaded: book.uploaded,
            authors: book.authors
        }
    }
}
