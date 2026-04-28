use std::io::Cursor;
use zip::ZipArchive;

fn main() {
    let mut archive = ZipArchive::new(Cursor::new(vec![])).unwrap();
    let mut entry = archive.by_index(0).unwrap();
    println!("{}", entry.name());
}
