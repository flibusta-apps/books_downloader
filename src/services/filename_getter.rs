use translit::{gost779b_ru, CharsMapping, Transliterator};

use super::book_library::types::{BookAuthor, BookWithRemote};

pub fn get_author_short_name(author: BookAuthor) -> String {
    let mut parts: Vec<String> = vec![];

    if !author.last_name.is_empty() {
        parts.push(author.last_name);
    }

    if !author.first_name.is_empty() {
        let first_char = author.first_name.chars().next().unwrap();
        parts.push(first_char.to_string());
    }

    if !author.middle_name.is_empty() {
        let first_char = author.middle_name.chars().next().unwrap();
        parts.push(first_char.to_string());
    }

    parts.join(" ")
}

pub fn get_filename_by_book(book: &BookWithRemote, file_type: &str, force_zip: bool, only_ascii: bool) -> String {
    let book_id = book.remote_id;
    let mut filename_parts: Vec<String> = vec![];

    let file_type_: String = if let "fb2zip" = file_type {
        "fb2.zip".to_string()
    } else if force_zip {
        format!("{file_type}.zip")
    } else {
        file_type.to_string()
    };

    if !book.authors.is_empty() {
        filename_parts.push(
            book.authors
                .clone()
                .into_iter()
                .map(get_author_short_name)
                .collect::<Vec<String>>()
                .join("_-_"),
        );
    }

    filename_parts.push(book.title.trim().to_string());

    let transliterator = Transliterator::new(gost779b_ru());
    let mut filename_without_type = transliterator.convert(&filename_parts.join("_"), false);

    "(),….’!\"?»«':".get(..).into_iter().for_each(|char| {
        filename_without_type = filename_without_type.replace(char, "");
    });

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
        ("[", ""),
        ("]", ""),
        ("\"", ""),
    ].to_vec();

    let replace_transliterator = Transliterator::new(replace_char_map);
    let mut normal_filename = replace_transliterator.convert(&filename_without_type, false);

    if only_ascii {
        normal_filename = normal_filename.replace(|c: char| !c.is_ascii(), "");
    }

    let right_part = format!(".{book_id}.{file_type_}");
    let normal_filename_slice = std::cmp::min(64 - right_part.len() - 1, normal_filename.len() -1);
    let left_part = normal_filename.get(..normal_filename_slice).unwrap_or_else(|| panic!("Can't slice left part: {:?} {:?}", normal_filename, normal_filename_slice));

    format!("{left_part}{right_part}")
}
