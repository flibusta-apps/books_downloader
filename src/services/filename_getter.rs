use translit::{gost779b_ru, CharsMapping, Transliterator};

use super::book_library::types::{BookAuthor, BookWithRemote};

pub fn get_author_short_name(author: BookAuthor) -> String {
    let mut parts: Vec<String> = vec![];

    if author.last_name.len() != 0 {
        parts.push(author.last_name);
    }

    if author.first_name.len() != 0 {
        let first_char = author.first_name.chars().next().unwrap();
        parts.push(first_char.to_string());
    }

    if author.middle_name.len() != 0 {
        let first_char = author.middle_name.chars().next().unwrap();
        parts.push(first_char.to_string());
    }

    parts.join(" ")
}

pub fn get_filename_by_book(book: &BookWithRemote, file_type: &str, force_zip: bool) -> String {
    let book_id = book.remote_id;
    let mut filename_parts: Vec<String> = vec![];

    let file_type_: String = if let "fb2zip" = file_type {
        "fb2.zip".to_string()
    } else if force_zip {
        format!("{file_type}.zip")
    } else {
        file_type.to_string()
    };

    filename_parts.push(
        book.authors
            .clone()
            .into_iter()
            .map(|author| get_author_short_name(author))
            .collect::<Vec<String>>()
            .join("_-_"),
    );
    filename_parts.push(book.title.trim().to_string());

    let transliterator = Transliterator::new(gost779b_ru());
    let mut filename_without_type = transliterator.convert(&filename_parts.join(""), false);

    for char in "(),….’!\"?»«':".get(..) {
        filename_without_type = filename_without_type.replace(char, "");
    }

    let replace_char_map: CharsMapping = [
        ("—", "-"),
        ("/", "_"),
        ("№", "N"),
        (" ", "_"),
        ("–", "-"),
        ("á", "a"),
        (" ", "_"),
        ("'", ""),
        ("`", ""),
    ]
    .iter()
    .cloned()
    .collect();

    let replace_transliterator = Transliterator::new(replace_char_map);
    let normal_filename = replace_transliterator.convert(&filename_without_type, false);

    let right_part = format!(".{book_id}.{file_type_}");
    let normal_filename_slice = std::cmp::min(64 - right_part.len() - 1, normal_filename.len());
    let left_part = normal_filename.get(..normal_filename_slice).unwrap();

    format!("{left_part}{right_part}")
}