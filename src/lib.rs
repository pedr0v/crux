mod index;
mod prepare;
mod prepare_java;
mod prepare_languages;
mod queries;
mod render;
mod semantic;
mod server;
mod setup;
#[cfg(test)]
mod test_support;
mod update;

pub use server::run;
