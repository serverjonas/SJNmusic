mod daemon;
mod socket;
mod protocol;
mod state;
mod db;
mod audio;
mod paths;

fn main() {
    daemon::run();
}
