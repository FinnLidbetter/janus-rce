pub mod auth;
pub mod config;
pub mod executor;
pub mod routes;
pub mod validate;

pub fn build_rocket(
    figment: rocket::figment::Figment,
    config: config::LoadedConfig,
) -> rocket::Rocket<rocket::Build> {
    rocket::custom(figment)
        .manage(config)
        .mount(
            "/",
            rocket::routes![routes::health, routes::commands, routes::run],
        )
        .register(
            "/",
            rocket::catchers![
                routes::unauthorized,
                routes::not_found,
                routes::unprocessable,
                routes::internal_error,
            ],
        )
}
