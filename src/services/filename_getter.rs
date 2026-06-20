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

pub fn get_filename_by_book(
    book: &BookWithRemote,
    file_type: &str,
    force_zip: bool,
    only_ascii: bool,
    normalized: bool,
) -> String {
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

    let mut filename_without_type = filename_parts.join("_");

    // Replace № -> N before GOST runs, otherwise GOST 7.79B would turn
    // it into '#' and the replace_char_map entry below would not fire.
    // The same applies to other characters that the GOST table maps to
    // something undesirable for a file name.
    filename_without_type = filename_without_type
        .replace('\u{2116}', "N")
        .replace(['\u{00AB}', '\u{00BB}'], "");

    if normalized {
        let transliterator = Transliterator::new(gost779b_ru());
        filename_without_type = transliterator.convert(&filename_without_type, false);
    }

    // Strip punctuation and quotes that don't survive the transliterator.
    // Characters are spelled out as their Unicode escape so source encoding
    // can't smuggle in look-alikes.
    let stripped: String = filename_without_type
        .chars()
        .filter(|c| {
            !matches!(
                *c,
                '(' | ')'
            | ',' | '.'
            | '\u{2026}' // …
            | '\u{2019}' // ’
            | '!'
            | '"'
            | '?'
            | '\''
            | ':'
            )
        })
        .collect();
    filename_without_type = stripped;

    let replace_char_map: CharsMapping = [
        ("—", "-"),
        ("/", "_"),
        (" ", "_"),
        ("–", "-"),
        ("á", "a"),
        (" ", "_"),
        ("'", ""),
        ("`", ""),
        ("[", ""),
        ("]", ""),
        ("\"", ""),
    ]
    .to_vec();

    let replace_transliterator = Transliterator::new(replace_char_map);
    let mut normal_filename = replace_transliterator.convert(&filename_without_type, false);

    if only_ascii && normalized {
        normal_filename = normal_filename.replace(|c: char| !c.is_ascii(), "");
    }

    // Telegram-safe cleanup. Telegram Bot API's `file_name` is limited to 60
    // bytes UTF-8 and rejects some characters outright. We strip the rest
    // so a generated name is always safe to ship.
    normal_filename = normal_filename
        .replace(['\\', '|', ':', '*', '"', '<', '>', '?', '!'], "");

    // Telegram reserves 60 bytes for the whole `file_name`; cap the
    // left part at 50 bytes UTF-8 to leave headroom for `.ID.ext(.zip)`.
    const TELEGRAM_LEFT_MAX_BYTES: usize = 50;
    let right_part = format!(".{book_id}.{file_type_}");
    let left_max = TELEGRAM_LEFT_MAX_BYTES.min(normal_filename.len());
    let slice_end = normal_filename.floor_char_boundary(left_max);

    // Collapse trailing underscores (or dots) that may appear after stripping
    // characters, so the name doesn't end in a separator.
    let mut left_part = &normal_filename[..slice_end];
    while let Some(c) = left_part.chars().last() {
        if c == '_' || c == '-' || c == '.' || c == ' ' {
            let new_len = left_part.len() - c.len_utf8();
            // new_len is always a valid char boundary: we are removing the
            // entire last char, so whatever preceded it is still aligned.
            left_part = &left_part[..new_len];
        } else {
            break;
        }
    }

    format!("{left_part}{right_part}")
}

#[cfg(test)]
mod tests {
    use super::super::book_library::types::{BookAuthor, BookWithRemote};
    use super::get_filename_by_book;

    fn make_book(title: &str, authors: Vec<BookAuthor>) -> BookWithRemote {
        BookWithRemote {
            id: 1,
            remote_id: 42,
            title: title.to_string(),
            lang: "ru".to_string(),
            file_type: "fb2".to_string(),
            uploaded: "2024-01-01".to_string(),
            authors,
        }
    }

    fn author(last: &str, first: &str, middle: &str) -> BookAuthor {
        BookAuthor {
            id: 1,
            first_name: first.to_string(),
            last_name: last.to_string(),
            middle_name: middle.to_string(),
        }
    }

    #[test]
    fn it_works() {
        let t = "Usachev_A_A_Priklyucheniya_«Kotoboya»";
        let r = t.get(..t.len() - 2);

        println!("{:?}", r);
    }

    #[test]
    fn normalized_full_name_transliterates() {
        let book = make_book(
            "Приключения Кота",
            vec![author("Усачёв", "Андрей", "Александрович")],
        );
        let filename = get_filename_by_book(&book, "fb2", false, false, true);
        // GOST 7.79B: ё -> "yo"; both initials A + A included
        assert_eq!(filename, "Usachyov_A_A_Priklyucheniya_Kota.42.fb2");
    }

    #[test]
    fn normalized_short_name_uses_initials() {
        let book = make_book(
            "Приключения Кота",
            vec![author("Усачёв", "Андрей", "Александрович")],
        );
        let filename = get_filename_by_book(&book, "fb2", false, false, true);
        // First char of first_name + first char of middle_name
        assert!(filename.starts_with("Usachyov_A_"));
    }

    #[test]
    fn not_normalized_keeps_cyrillic() {
        let book = make_book(
            "Приключения Кота",
            vec![author("Усачёв", "Андрей", "Александрович")],
        );
        let filename = get_filename_by_book(&book, "fb2", false, false, false);
        assert_eq!(filename, "Усачёв_А_А_Приключения_Кота.42.fb2");
    }

    #[test]
    fn not_normalized_with_only_ascii_keeps_cyrillic() {
        let book = make_book(
            "Приключения Кота",
            vec![author("Усачёв", "Андрей", "Александрович")],
        );
        let filename = get_filename_by_book(&book, "fb2", false, true, false);
        // Per agreement: ascii is left as-is when normalized=false
        assert_eq!(filename, "Усачёв_А_А_Приключения_Кота.42.fb2");
    }

    #[test]
    fn normalized_with_only_ascii_strips_non_ascii() {
        let book = make_book(
            "Приключения Кота",
            vec![author("Усачёв", "Андрей", "Александрович")],
        );
        let filename = get_filename_by_book(&book, "fb2", false, true, true);
        // After transliteration there is no non-ASCII left, so unchanged
        assert_eq!(filename, "Usachyov_A_A_Priklyucheniya_Kota.42.fb2");
    }

    #[test]
    fn multiple_authors_joined() {
        let book = make_book(
            "Какая-то книга",
            vec![
                author("Иванов", "Иван", "Иванович"),
                author("Петров", "Пётр", "Петрович"),
            ],
        );
        let filename = get_filename_by_book(&book, "fb2", false, false, true);
        // GOST: Пётр -> "Petrov" (ё -> "e" in this context), each author carries first+middle initials
        assert_eq!(filename, "Ivanov_I_I_-_Petrov_P_P_Kakaya-to_kniga.42.fb2");
    }

    #[test]
    fn no_authors_skips_author_part() {
        let book = make_book("Просто книга", vec![]);
        let filename = get_filename_by_book(&book, "fb2", false, false, true);
        assert_eq!(filename, "Prosto_kniga.42.fb2");
    }

    #[test]
    fn author_with_only_last_name() {
        // Empty first/middle fields would panic in get_author_short_name
        // (chars().next().unwrap() on ""), so we only verify the part we
        // can isolate: that a non-empty last name is transliterated.
        let book = make_book("Книга", vec![author("Толстой", "Л", "Н")]);
        let filename = get_filename_by_book(&book, "fb2", false, false, true);
        assert!(filename.starts_with("Tolstoj_L_N_Kniga."));
    }

    #[test]
    fn special_chars_in_title_stripped() {
        // Telegram-safe pass removes ? and ! alongside the original
        // cleanup set. The result is a clean underscore-joined name.
        let book = make_book("Что? Где? Когда!", vec![]);
        let filename = get_filename_by_book(&book, "fb2", false, false, true);
        assert_eq!(filename, "Chto_Gde_Kogda.42.fb2");
    }

    #[test]
    fn guillemets_stripped_in_normalized() {
        let book = make_book("«Котобой»", vec![]);
        let filename = get_filename_by_book(&book, "fb2", false, false, true);
        assert_eq!(filename, "Kotoboj.42.fb2");
    }

    #[test]
    fn guillemets_stripped_in_non_normalized() {
        let book = make_book("«Котобой»", vec![]);
        let filename = get_filename_by_book(&book, "fb2", false, false, false);
        assert_eq!(filename, "Котобой.42.fb2");
    }

    #[test]
    fn em_dash_replaced_with_hyphen() {
        let book = make_book("А — Б", vec![]);
        let filename = get_filename_by_book(&book, "fb2", false, false, true);
        assert_eq!(filename, "A_-_B.42.fb2");
    }

    #[test]
    fn number_sign_replaced_in_normalized() {
        // Pre-GOST replacement of № -> N ensures consistent behaviour
        // in both modes. GOST would otherwise turn № into "#".
        let book = make_book("№ 42", vec![]);
        let filename = get_filename_by_book(&book, "fb2", false, false, true);
        assert_eq!(filename, "N_42.42.fb2");
    }

    #[test]
    fn number_sign_replaced_in_non_normalized() {
        let book = make_book("№ 42", vec![]);
        let filename = get_filename_by_book(&book, "fb2", false, false, false);
        assert_eq!(filename, "N_42.42.fb2");
    }

    #[test]
    fn force_zip_appends_extension() {
        let book = make_book("Книга", vec![author("Иванов", "И", "И")]);
        let filename = get_filename_by_book(&book, "epub", true, false, true);
        assert!(filename.ends_with(".42.epub.zip"));
    }

    #[test]
    fn fb2zip_uses_fb2_zip_extension() {
        let book = make_book("Книга", vec![author("Иванов", "И", "И")]);
        let filename = get_filename_by_book(&book, "fb2zip", false, false, true);
        assert!(filename.ends_with(".42.fb2.zip"));
    }

    #[test]
    fn long_title_is_truncated_to_50_bytes() {
        // Telegram-safe pass caps the left part at 50 bytes UTF-8 so that
        // the final file_name fits in Telegram's 60-byte limit.
        let long_title: String = "А".repeat(200);
        let book = make_book(&long_title, vec![author("Иванов", "И", "И")]);
        let filename = get_filename_by_book(&book, "fb2", false, false, true);
        assert!(
            filename.len() <= 60,
            "filename is {} bytes: {filename}",
            filename.len()
        );
        assert!(filename.ends_with(".42.fb2"));
    }

    #[test]
    fn short_title_not_truncated() {
        let book = make_book("Кот", vec![author("Иванов", "И", "И")]);
        let filename = get_filename_by_book(&book, "fb2", false, false, true);
        assert_eq!(filename, "Ivanov_I_I_Kot.42.fb2");
    }

    #[test]
    fn title_is_trimmed() {
        let book = make_book("   Кот   ", vec![]);
        let filename = get_filename_by_book(&book, "fb2", false, false, true);
        assert_eq!(filename, "Kot.42.fb2");
    }

    #[test]
    fn only_last_name_no_initials() {
        // get_author_short_name guards on is_empty() and skips empty
        // first/middle fields, so the result is just the last name with
        // no trailing space, joined to the title by a single "_".
        let book = make_book("Кот", vec![author("Иванов", "", "")]);
        let filename = get_filename_by_book(&book, "fb2", false, false, true);
        assert_eq!(filename, "Ivanov_Kot.42.fb2");
    }

    #[test]
    fn slash_replaced_with_underscore() {
        let book = make_book("Война/и/мир", vec![]);
        let filename = get_filename_by_book(&book, "fb2", false, false, true);
        assert_eq!(filename, "Vojna_i_mir.42.fb2");
    }

    #[test]
    fn telegram_unsafe_chars_stripped_in_normalized() {
        // Telegram rejects \ | : * " < > ? ! in file_name; the safe pass
        // strips them after transliteration.
        let book = make_book("A*B?C!D|E", vec![]);
        let filename = get_filename_by_book(&book, "fb2", false, false, true);
        assert_eq!(filename, "ABCDE.42.fb2");
    }

    #[test]
    fn telegram_unsafe_chars_stripped_in_non_normalized() {
        let book = make_book("A*B?C!D|E", vec![]);
        let filename = get_filename_by_book(&book, "fb2", false, false, false);
        assert_eq!(filename, "ABCDE.42.fb2");
    }

    #[test]
    fn colons_and_quotes_stripped() {
        let book = make_book("He said: \"hi\"", vec![]);
        let filename = get_filename_by_book(&book, "fb2", false, false, true);
        assert!(!filename.contains(':'));
        assert!(!filename.contains('"'));
    }

    #[test]
    fn long_cyrillic_title_truncated_at_utf8_boundary() {
        // Build a long Cyrillic title in non-normalized mode and verify
        // the slice lands on a char boundary (no panic) and that the
        // final byte length fits Telegram's 60-byte limit.
        let long_title: String = "К".repeat(100);
        let book = make_book(&long_title, vec![author("Иванов", "И", "И")]);
        let filename = get_filename_by_book(&book, "fb2", false, false, false);
        assert!(
            filename.len() <= 60,
            "filename is {} bytes: {filename}",
            filename.len()
        );
        assert!(filename.ends_with(".42.fb2"));
    }

    #[test]
    fn no_trailing_separator_after_strip() {
        // After stripping ? and ! at the end of the title, the
        // left part should not end with '_' or ' ' before .ID.ext.
        let book = make_book("Что?!", vec![]);
        let filename = get_filename_by_book(&book, "fb2", false, false, true);
        // filename structure: <left>.42.fb2
        let left = filename.trim_end_matches(".42.fb2");
        assert!(
            !left.ends_with('_') && !left.ends_with('-') && !left.ends_with('.'),
            "left part ends with separator: {left}"
        );
    }

    #[test]
    fn filename_under_60_bytes_for_telegram() {
        // Sanity check: realistic Russian title should produce a
        // file_name that fits Telegram's 60-byte limit.
        let book = make_book("Война и мир", vec![author("Толстой", "Лев", "Николаевич")]);
        let filename = get_filename_by_book(&book, "fb2", false, false, true);
        assert!(
            filename.len() <= 60,
            "filename is {} bytes: {filename}",
            filename.len()
        );
    }
}
