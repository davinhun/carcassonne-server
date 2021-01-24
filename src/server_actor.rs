//!
//! This package is the central server that controls all the rooms.
//! Every client websocket should send us an event with each action they take.
//!
//! This makes the code quite simple as it doesn't need to comply with any async strangeness but
//! it creates a bottleneck as every single event in all of the server passes trough a single thread.
//! (stress performance test needed). In addition the delay between packet sharing between threads
//! adds up.
//!
//! Additional work is being done to decentralize this, replacing it with a
//!

use std::{collections::{HashMap, HashSet}};

use actix::dev::{MessageResponse, ResponseChannel};
use actix::prelude::*;
use rand::{self, Rng, rngs::ThreadRng};

use crate::client_ws::ClientWs;
use crate::protocol::{IdType, LoginData, OutEvent, OutGameEvent, PlayerCosmetics, PlayerObject, RoomConnectionType, SerId};

// Copied from actix, love the library but it seems a bit rushed in the "actor" part.
// This should generate the code to share a result between actors.
macro_rules! simple_result {
    ($type:ty) => {
        impl<A, M> MessageResponse<A, M> for $type
        where
            A: Actor,
            M: Message<Result = $type>,
        {
            fn handle<R: ResponseChannel<M>>(self, _: &mut A::Context, tx: Option<R>) {
                if let Some(tx) = tx {
                    tx.send(self);
                }
            }
        }
    };
}


#[derive(Message)]
#[rtype(result = "()")]
pub struct Event(pub OutEvent);

#[derive(Message)]
#[rtype(result = "()")]
pub struct GameEvent(pub OutGameEvent);

#[derive(Message)]
#[rtype(IdType)]
pub struct RegisterSession {
    pub id: Option<IdType>,
    pub addr: Addr<ClientWs>,
    pub obj: LoginData,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct Disconnect {
    pub id: IdType,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct EditCosmetics {
    pub id: IdType,
    pub obj: PlayerCosmetics,
}

// ----------------------------------------------------------------

#[derive(Message)]
#[rtype(FindRoomResult)]
pub struct FindRoom {
    pub id: IdType
}

pub enum FindRoomResult {
    Success {
        room_id: IdType, 
        players: Vec<PlayerObject>,
        just_created: bool
    }, 
    GameIsFull,
}

simple_result!(FindRoomResult);

// ----------------------------------------------------------------

#[derive(Message)]
#[rtype(CreateRoomResult)]
pub struct CreateRoom {
    pub id: IdType,
}

pub struct CreateRoomResult {
    pub room_id: IdType,
    pub player: PlayerObject,
}

simple_result!(CreateRoomResult);

#[derive(Message)]
#[rtype(JoinRoomResult)]
pub struct JoinRoom {
    pub id: IdType,
    pub room_id: IdType,
}

pub enum JoinRoomResult {
    Success(Vec<PlayerObject>),
    RoomNotFound,
    AlreadyPlaying,
}
simple_result!(JoinRoomResult);

#[derive(Message)]
#[rtype(result = "()")]
pub struct LeaveRoom {
    pub id: IdType,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct StartRoom {
    pub id: IdType,
    pub conn_type: RoomConnectionType,
}

#[derive(Message, Clone)]
#[rtype(result = "()")]
pub struct SendRelayMex {
    pub sender_id: IdType,
    pub data: String,
}

#[derive(Message, Clone)]
#[rtype(result = "()")]
pub struct SendRelayMexRaw {
    pub data: String,
}

#[derive(Message, Clone)]
#[rtype(result = "Option<GameEndAck>")]
pub struct GameEndRequest {
    pub id: IdType,
}

pub struct GameEndAck(pub Vec<PlayerObject>);
simple_result!(GameEndAck);


struct UserData {
    addr: Addr<ClientWs>,
    obj: PlayerObject,
    room: Option<IdType>,
    in_game: bool,
}

struct RoomData {
    state: RoomState,
    players: HashSet<IdType>,
    in_game_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomState {
    Matchmaking,
    Playing
}


pub struct ServerActor {
    players: HashMap<IdType, UserData>,
    rooms: HashMap<IdType, RoomData>,
    available_rooms: HashSet<IdType>,
    rng: ThreadRng,
}

impl Default for ServerActor {
    fn default() -> Self {
        ServerActor {
            players: HashMap::new(),
            rooms: HashMap::new(),
            available_rooms: HashSet::new(),
            rng: rand::thread_rng(),
        }
    }
}

impl Actor for ServerActor {
    /// We are going to use simple Context, we just need ability to communicate
    /// with other actors.
    type Context = Context<Self>;
}

impl ServerActor {
    fn allocate_player_id(&mut self, mut data: UserData) -> IdType {
        let mut id;

        loop {
            id = self.rng.gen::<IdType>();

            if !self.players.contains_key(&id) {
                break;
            }
        }
        data.obj.id = id.into();
        self.players.insert(id, data);
        id
    }

    fn create_room(&mut self, host_id: IdType, public: bool) -> IdType {
        let mut id;

        loop {
            id = self.rng.gen::<IdType>();

            if !self.rooms.contains_key(&id) {
                break;
            }
        }

        let mut players = HashSet::new();
        players.insert(host_id);
        let room = RoomData {
            state: RoomState::Matchmaking,
            players,
            in_game_count: 0,
        };
        self.rooms.insert(id, room);

        let host = self.players.get_mut(&host_id).unwrap();
        host.obj.is_host = true;
        host.room = Some(id);

        if public {
            self.available_rooms.insert(id);
        }

        id
    }

    fn remove_room(&mut self, room_id: IdType) {
        self.rooms.remove(&room_id);
        self.available_rooms.remove(&room_id);

        //println!("room removed (id={}) because it's empty", room_id);
    }

    fn join_room(&mut self, player_id: IdType, room_id: IdType) -> bool {
        self.leave_room_if_any(player_id); // If the player was already inside a room, makes him quit.

        self.rooms.get_mut(&room_id).unwrap().players.insert(player_id); // Adds the player to the target room.

        let room_data = self.rooms.get(&room_id).expect("Cannot find room");
        if room_data.state != RoomState::Matchmaking { // The room isn't in the correct state, it can't be joined.
            return false;
        }

        let user_data = self.players.get_mut(&player_id).expect("Cannot find player");
        user_data.room = Some(room_id); // Saves that the player is connected to this room.

        let player_obj = user_data.obj.clone();
        self.broadcast_event_room(&room_data, OutEvent::EventPlayerJoined { // Finally broadcasts that the player joined to all players in the room.
            player: player_obj
        }, None);

        true
    }

    /// Send event to all users in the room
    fn broadcast_event(&self, room: IdType, event: OutEvent, skip_id: Option<IdType>) {
        match self.rooms.get(&room) {
            Some(room) => self.broadcast_event_room(room, event, skip_id),
            None => {},
        };
    }

    fn broadcast_event_room(&self, room: &RoomData, event: OutEvent, skip_id: Option<IdType>) {
        for id in room.players.iter() {
            if Some(*id) == skip_id {
                continue;
            }
            let player = match self.players.get(&id) {
                Some(x) => x,
                None => continue,
            };

            if player.in_game {
                continue; // Don't send if player is still in the game.
            }
            player.addr.do_send(Event(event.clone()));// TODO: remove clone
        }
    }

    fn leave_room_if_any(&mut self, player_id: IdType) {
        let player = match self.players.get_mut(&player_id) {
            Some(x) => x,
            None => return,
        };
        let room_id = match player.room {
            Some(x) => x,
            None => return,
        };

        let room = self.rooms.get_mut(&room_id).expect("Cannot find room");
        room.players.remove(&player_id);

        if player.in_game {
            room.in_game_count -= 1;
        }

        let was_player_host = player.obj.is_host;
        player.room = None;
        player.obj.is_host = false;

        if let Some(first_player) = room.players.iter().next() {
            let new_host = if was_player_host {
                let mut p = self.players.get_mut(first_player).expect("Invalid player");
                p.obj.is_host = true;
                Some(p.obj.id)
            } else {
                None
            };

            // Why cant I convert a mutable reference to an immutable one? wtf
            // let room = &*room;
            //let room = self.rooms.get(&room_id).unwrap();

            let event = OutEvent::EventPlayerLeft {
                player: player_id.into(),
                new_host,
            };

            let in_game_event = OutGameEvent::PlayerLeft {
                player: player_id.into(),
                new_host
            };

            for id in room.players.iter() {
                let player = match self.players.get(&id) {
                    Some(x) => x,
                    None => continue,
                };

                if player.in_game {
                    player.addr.do_send(GameEvent(in_game_event.clone()));
                } else {
                    player.addr.do_send(Event(event.clone()));// TODO: remove clone
                }
            }
        } else {
            self.remove_room(room_id);
        }
    }

    fn find_available_room_for(&mut self, player_id: IdType, find_if: impl Fn(IdType, &RoomData) -> bool, max_iter: i32) -> Option<IdType> {
        let mut found = false;
        let mut found_room_id = 0;

        let mut iter = 0;
        for room_id in &self.available_rooms {
            if max_iter > 0 && iter >= max_iter {
                break;
            }
            let room_data = self.rooms.get(&room_id).unwrap();
            if find_if(*room_id, room_data) {
                found = true;
                found_room_id = *room_id;
            }
            iter += 1;
        }

        if found {
            self.rooms.get_mut(&found_room_id).unwrap().players.insert(player_id);
            Some(found_room_id)
        } else {
            None
        }
    }
}

impl Handler<RegisterSession> for ServerActor {
    type Result = IdType;

    fn handle(&mut self, msg: RegisterSession, _: &mut Context<Self>) -> Self::Result {
        match msg.id {
            Some(id) => {
                let player = self.players.get_mut(&id).expect("Invalid player");
                if player.room.is_none() {
                    player.obj.username = msg.obj.username;
                    player.obj.cosmetics = msg.obj.cosmetics;
                }
                id
            },
            None => {
                let pobj = PlayerObject {
                    id: 0.into(),
                    username: msg.obj.username,
                    cosmetics: msg.obj.cosmetics,
                    is_host: false
                };
                self.allocate_player_id(UserData {
                    addr: msg.addr,
                    obj: pobj,
                    room: None,
                    in_game: false,
                })
            }
        }

    }
}

impl Handler<Disconnect> for ServerActor {
    type Result = ();

    fn handle(&mut self, msg: Disconnect, _: &mut Context<Self>) -> Self::Result {
        self.leave_room_if_any(msg.id);
        self.players.remove(&msg.id);
    }
}

impl Handler<FindRoom> for ServerActor {
    type Result = FindRoomResult;

    fn handle(&mut self, msg: FindRoom, _: &mut Context<Self>) -> Self::Result {
        let my_id = msg.id;

        let mut just_created = false;

        let room_id = self.find_available_room_for(
            my_id, 
            |_, _| { true }, 
            -1
        );

        let room_id = match room_id {
            Some(room_id) => {
                self.join_room(my_id, room_id);
                room_id
            },
            None => {
                just_created = true;
                self.create_room(my_id, true)
            }
        };

        FindRoomResult::Success {
            room_id,
            players: self.rooms.get(&room_id)
                .unwrap()
                .players
                .iter()
                .map(|x| self.players.get(x).expect("Cannot find player").obj.clone())
                .collect(),
            just_created
        }
    }
}

impl Handler<CreateRoom> for ServerActor {
    type Result = CreateRoomResult;

    fn handle(&mut self, msg: CreateRoom, _: &mut Context<Self>) -> Self::Result {
        self.leave_room_if_any(msg.id);
        let room_id = self.create_room(msg.id, false);
        let player = self.players.get_mut(&msg.id).expect("Cannot find player");
        CreateRoomResult {
            room_id,
            player: player.obj.clone()
        }
    }
}

impl Handler<JoinRoom> for ServerActor {
    type Result = JoinRoomResult;

    fn handle(&mut self, msg: JoinRoom, _: &mut Context<Self>) -> Self::Result {

        let player_id = msg.id;
        let room_id = msg.room_id;

        if !self.rooms.contains_key(&room_id) {
            return JoinRoomResult::RoomNotFound;
        }

        let result = self.join_room(player_id, room_id);
        if !result {
            return JoinRoomResult::AlreadyPlaying;
        }

        let users = self.rooms.get(&msg.room_id)
            .unwrap()
            .players
            .iter()
            .map(|x| self.players.get(x).expect("Cannot find player").obj.clone())
            .collect();
        JoinRoomResult::Success(users)
    }
}

impl Handler<EditCosmetics> for ServerActor {
    type Result = ();

    fn handle(&mut self, msg: EditCosmetics, _: &mut Context<Self>) -> Self::Result {
        let player = self.players.get_mut(&msg.id).expect("Invalid player");

        if player.obj.cosmetics == msg.obj {
            return;
        }
        player.obj.cosmetics = msg.obj.clone();

        let room = match player.room {
            Some(x) => x,
            None => return,
        };

        let id = player.obj.id;

        self.broadcast_event(room, OutEvent::EventPlayerAvatarChange {
            player: id.into(),
            cosmetics: msg.obj,
        }, Some(msg.id));
    }
}

impl Handler<LeaveRoom> for ServerActor {
    type Result = ();

    fn handle(&mut self, msg: LeaveRoom, _: &mut Context<Self>) -> Self::Result {
        self.leave_room_if_any(msg.id);
    }
}

impl Handler<StartRoom> for ServerActor {
    type Result = ();

    fn handle(&mut self, msg: StartRoom, _: &mut Context<Self>) -> Self::Result {
        let room_id = match self.players.get(&msg.id).and_then(|x| x.room) {
            Some(x) => x,
            None => return,
        };

        if let Some(room) = self.rooms.get_mut(&room_id) {

            self.available_rooms.remove(&room_id);

            if room.state != RoomState::Matchmaking || room.players.len() < 2 {
                return
            }

            room.state = RoomState::Playing;

            let event = OutEvent::EventRoomStart {
                connection_type: msg.conn_type,
                broadcast_id: format!("{}", room_id)
            };

            let room = if room.in_game_count > 0 {
                // Kick players that are still in-game
                let mut in_game_players = vec![];
                for id in room.players.iter() {
                    if let Some(x) = self.players.get_mut(&id) {
                        if x.in_game {
                            in_game_players.push(*id);
                        }
                    }
                }

                for id in in_game_players {
                    self.leave_room_if_any(id);
                }

                match self.rooms.get_mut(&room_id) {
                    None => return,
                    Some(x) => x,
                }
            } else {
                room
            };

            for id in room.players.iter() {
                if let Some(x) = self.players.get_mut(&id) {
                    x.in_game = true;
                    let _ = x.addr.do_send(Event(event.clone()));// TODO: remove clone
                }
            }
            room.in_game_count = room.players.len() as u32;
        }
    }
}

impl Handler<SendRelayMex> for ServerActor {
    type Result = ();

    fn handle(&mut self, msg: SendRelayMex, _ctx: &mut Context<Self>) -> Self::Result {
        // TODO: do not clone.
        // it's better to create a queue with multiple indexes
        // A B C D E
        //^     ^   ^
        //p1    p2  p4
        //      p3
        if msg.data.is_empty() {
            return;
        }

        let player = self.players.get(&msg.sender_id).expect("Expected player");
        let room = match player.room.and_then(|room| self.rooms.get(&room)) {
            Some(x) => x,
            None => return,
        };

        let raw = format!("{{\"sender\":\"{}\",{}", SerId(msg.sender_id), &msg.data[1..]);
        let raw_pkt = SendRelayMexRaw { data: raw };
        for player in room.players.iter() {
            if *player == msg.sender_id {
                continue;
            }
            let player = match self.players.get(&player) {
                Some(x) => x,
                None => continue,
            };
            if player.in_game {
                player.addr.do_send(raw_pkt.clone())
            }
        }
    }
}

impl Handler<GameEndRequest> for ServerActor {
    type Result = Option<GameEndAck>;

    fn handle(&mut self, msg: GameEndRequest, _ctx: &mut Context<Self>) -> Self::Result {
        let player = self.players.get_mut(&msg.id).expect("Invalid player");
        let rooms = &mut self.rooms;
        let room = match player.room.and_then(|x| rooms.get_mut(&x)) {
            Some(x) => x,
            None => return None,
        };

        if !player.in_game {
            return None;
        }

        room.state = RoomState::Matchmaking;
        player.in_game = false;
        room.in_game_count -= 1;

        let room = self.rooms.get(&player.room.unwrap()).unwrap();

        let users = room.players.iter()
            .map(|x| self.players.get(x).expect("Cannot find player").obj.clone())
            .collect();

        return Some(GameEndAck(users));
    }
}

