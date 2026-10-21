fn main() {
    let ec = msi_control_core::ec::Ec::system();
    println!("{:#?}", ec.state());
    println!("{:#?}", msi_control_core::mux::read());
}
