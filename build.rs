fn main() {
    embed_resource::compile("studiodeck.rc", embed_resource::NONE).manifest_optional().unwrap();
}
