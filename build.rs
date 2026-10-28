fn main() {
    println!("cargo:rerun-if-changed=skill");
    println!("cargo:rerun-if-changed=templates");
}
