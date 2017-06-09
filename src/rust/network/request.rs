use std::borrow::Cow;

use hyper;
use futures::{future, Future};
use screeps_api::{self, NoToken};
use tokio_core::reactor;

use self::Request::*;

use super::LoginDetails;
use super::cache::disk;

#[derive(Clone, Debug, Hash)]
pub enum Request {
    Login { details: LoginDetails },
    MyInfo,
    RoomTerrain { room_name: screeps_api::RoomName },
}

impl Request {
    pub fn login<'a, T, U>(username: T, password: U) -> Self
        where T: Into<Cow<'a, str>>,
              U: Into<Cow<'a, str>>
    {
        Login { details: LoginDetails::new(username.into().into_owned(), password.into().into_owned()) }
    }

    pub fn login_with_details(details: LoginDetails) -> Self {
        Login { details: details }
    }


    pub fn my_info() -> Self {
        Request::MyInfo
    }

    pub fn room_terrain(room_name: screeps_api::RoomName) -> Self {
        Request::RoomTerrain { room_name: room_name }
    }

    pub fn exec_with<C, H, T>(&self,
                              login: &LoginDetails,
                              client: &screeps_api::Api<C, H, T>,
                              cache: &disk::Cache,
                              handle: &reactor::Handle)
                              -> Box<Future<Item = NetworkEvent, Error = ()> + 'static>
        where C: hyper::client::Connect,
              H: screeps_api::HyperClient<C> + Clone + 'static,
              T: screeps_api::TokenStorage
    {
        match *self {
            Login { ref details } => {
                let tokens = client.tokens.clone();
                let details = details.clone();
                Box::new(client.login(details.username(), details.password())
                    .then(move |result| {
                        future::ok(NetworkEvent::Login {
                            username: details.username().to_owned(),
                            result: result.map(|logged_in| logged_in.return_to(&tokens)),
                        })
                    }))
            }
            MyInfo => {
                match client.my_info() {
                    Ok(future) => Box::new(future.then(|result| future::ok(NetworkEvent::MyInfo { result: result }))),
                    Err(NoToken) => {
                        let client = client.clone();
                        Box::new(client.login(login.username(), login.password())
                            .and_then(move |login_ok| {
                                login_ok.return_to(&client.tokens);

                                // TODO: something here to avoid a race condition!
                                client.my_info().expect("just returned token")
                            })
                            .then(|result| future::ok(NetworkEvent::MyInfo { result: result })))
                    }
                }
            }
            RoomTerrain { room_name } => {
                let client = client.clone();
                let cache = cache.clone();
                let handle = handle.clone();
                Box::new(cache.get_terrain(room_name).then(move |result| {
                    match result {
                            Ok(Some(terrain)) => {
                                Box::new(future::ok(terrain)) as
                                Box<Future<Item = screeps_api::TerrainGrid, Error = screeps_api::Error>>
                            }
                            other => {
                                if let Err(e) = other {

                                    warn!("error occurred fetching terrain cache: {}", e);
                                }
                                Box::new(client.room_terrain(room_name.to_string())
                                    .map(|data| data.terrain)
                                    .and_then(move |data| {
                                        handle.spawn(cache.set_terrain(room_name, &data)
                                            .then(|result| {
                                                if let Err(e) = result {
                                                    warn!("error occurred storing to terrain cache: {}", e);
                                                }
                                                Ok(())
                                            }));

                                        future::ok(data)
                                    })) as
                                Box<Future<Item = screeps_api::TerrainGrid, Error = screeps_api::Error>>
                            }
                        }
                        .then(move |result| {
                            future::ok(NetworkEvent::RoomTerrain {
                                room_name: room_name,
                                result: result,
                            })
                        })
                }))
            }
        }
    }
}

#[derive(Debug)]
pub enum NetworkEvent {
    Login {
        username: String,
        result: Result<(), screeps_api::Error>,
    },
    MyInfo { result: Result<screeps_api::MyInfo, screeps_api::Error>, },
    RoomTerrain {
        room_name: screeps_api::RoomName,
        result: Result<screeps_api::TerrainGrid, screeps_api::Error>,
    },
}


impl NetworkEvent {
    pub fn error(&self) -> Option<&screeps_api::Error> {
        match *self {
            NetworkEvent::Login { ref result, .. } => result.as_ref().err(),
            NetworkEvent::MyInfo { ref result, .. } => result.as_ref().err(),
            NetworkEvent::RoomTerrain { ref result, .. } => result.as_ref().err(),
        }
    }
}
