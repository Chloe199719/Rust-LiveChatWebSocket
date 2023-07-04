// #![deny(warnings)]
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use futures_util::{SinkExt, StreamExt, TryFutureExt};
use tokio::sync::{mpsc, RwLock};
use tokio_stream::wrappers::UnboundedReceiverStream;
use warp::ws::{Message, WebSocket};
use warp::Filter;

/// Our global unique user id counter.
static NEXT_USER_ID: AtomicUsize = AtomicUsize::new(1);

/// Our state of currently connected users.
///
/// - Key is their id
/// - Value is a sender of `warp::ws::Message`
type Users = Arc<RwLock<HashMap<String, mpsc::UnboundedSender<Message>>>>;
type History = Arc<RwLock<Vec<String>>>;

#[tokio::main]
async fn main() {
    pretty_env_logger::init();
    let history = Arc::new(RwLock::new(Vec::new()));
    // Keep track of all connected users, key is usize, value
    // is a websocket sender.
    let users = Users::default();
    // Turn our "state" into a new Filter...
    let users = warp::any().map(move || users.clone());
    let history = warp::any().map(move || history.clone());

    // GET /chat -> websocket upgrade
    let chat = warp::path("chat")
        // The `ws()` filter will prepare Websocket handshake...
        .and(warp::ws())
        .and(users)
        .and(history)
        .map(|ws: warp::ws::Ws, users, history| {
            // This will call our function if the handshake succeeds.
            ws.on_upgrade(move |socket| user_connected(socket, users, history))
        });

    // GET / -> index html
    let index = warp::path::end().map(|| warp::reply::html(INDEX_HTML));

    let routes = index.or(chat);

    warp::serve(routes).run(([127, 0, 0, 1], 3030)).await;
}

async fn user_connected(ws: WebSocket, users: Users, history: History) {
    // Use a counter to assign a new unique ID for this user.
    
    let my_id = NEXT_USER_ID.fetch_add(1, Ordering::Relaxed);

    eprintln!("new chat user: {}", my_id);

    // Split the socket into a sender and receive of messages.
    let (mut user_ws_tx, mut user_ws_rx) = ws.split();

    // let history1 = history.read().await;
    // if !history1.is_empty() {
    //     // Build the history string without holding the lock.
    //     let history_string = {
    //         let mut s = "History:\n".to_string();
    //         for item in history1.iter() {
    //             s.push_str(item);
    //             s.push('\n');
    //         }
    //         s
    //     };
    
    //     // Now send the history string to the client.
    //     if let Err(e) = user_ws_tx.send(Message::text(history_string)).await {
    //         eprintln!("websocket send error: {}", e);
    //     }
    // }
   
    let user_name = match user_ws_rx.next().await {
        Some(Ok(data)) => {
            if let Ok(data_str) = data.to_str() {
                Some(data_str.to_string())
            } else {
                eprintln!("Error converting message to string");
                None
            }
        }
        Some(Err(e)) => {
            eprintln!("Error receiving message: {}", e);
            None
        }
        None => {
            eprintln!("No message received from client");
            None
        }
    };
    let mut data= user_name.clone().unwrap_or_else(|| "Anonymous".to_string());
    if data.is_empty(){
        data = "Anonymous".to_string();
    }
   
    // If a user name was received, welcome them and send them the history
    if let Some(user_name) = user_name {
        
        user_ws_tx.send(Message::text(format!("Welcome to the chat, {}!", if user_name.is_empty() { "Anonymous" } else { &user_name })))
            .await
            .unwrap_or_else(|e| {
                eprintln!("websocket send error: {}", e);
            });
    
        // Now send the chat history
        let history1 = history.read().await;
        if !history1.is_empty() {
            let history_string = "History:\n".to_string();  
            user_ws_tx.send(Message::text(history_string))
                .await
                .unwrap_or_else(|e| {
                    eprintln!("websocket send error: {}", e);
                });
            for item in history1.iter() {
                // history_string.push_str(item);
                // history_string.push('\n');
                user_ws_tx.send(Message::text(item.to_string()))
                .await
                .unwrap_or_else(|e| {
                    eprintln!("websocket send error: {}", e);
                });
            }
          
        }
    }
    
    // Use an unbounded channel to handle buffering and flushing of messages
    // to the websocket...
    let (tx, rx) = mpsc::unbounded_channel();
    let mut rx = UnboundedReceiverStream::new(rx);

    tokio::task::spawn(async move {
        while let Some(message) = rx.next().await {
            user_ws_tx
                .send(message)
                .unwrap_or_else(|e| {
                    eprintln!("websocket send error: {}", e);
                })
                .await;
        }
    });

    // Save the sender in our list of connected users.
    users.write().await.insert(data.to_string(), tx);

    // Return a `Future` that is basically a state machine managing
    // this specific user's connection.

    // Every time the user sends a message, broadcast it to
    // all other users...
    while let Some(result) = user_ws_rx.next().await {
        let msg = match result {
            Ok(msg) => msg,
            Err(e) => {
                eprintln!("websocket error(uid={}): {}", my_id, e);
                break;
            }
        };
        user_message(data.to_string(), msg, &users, &history).await;
    }

    // user_ws_rx stream will keep processing as long as the user stays
    // connected. Once they disconnect, then...
    user_disconnected(data.to_string(), &users).await;
}

async fn user_message(my_id: String, msg: Message, users: &Users, history:  &History) {
    // Skip any non-Text messages...
    let msg = if let Ok(s) = msg.to_str() {
        s
    } else {
        return;
    };

    let new_msg = format!("<User#{}>: {}", my_id, msg);
    {
        let mut history_write = history.write().await;
        if history_write.len() >= 20 {
            // Remove the oldest message if there are already 20 messages.
            history_write.remove(0);
        }
        // Append the new message.
        history_write.push(new_msg.clone());
    }
    // New message from this user, send it to everyone else (except same uid)...
    for (uid, tx) in users.read().await.iter() {
        if my_id != *uid {
            if let Err(_disconnected) = tx.send(Message::text(new_msg.clone())) {
                // The tx is disconnected, our `user_disconnected` code
                // should be happening in another task, nothing more to
                // do here.
            }
        }
    }
}

async fn user_disconnected(my_id: String, users: &Users) {
    eprintln!("good bye user: {}", my_id);

    // Stream closed up, so remove from the user list
    users.write().await.remove(&my_id);
}

static INDEX_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
    <head>
        <title>Warp Chat</title>
    </head>
    <body>
        <h1>Warp chat</h1>
        <div id="chat">
            <p><em>Connecting...</em></p>
        </div>
        <input type="text" id="text" />
        <button type="button" id="send">Send</button>
        <script type="text/javascript">
        const chat = document.getElementById('chat');
        const text = document.getElementById('text');
        const uri = 'ws://' + location.host + '/chat';
        const ws = new WebSocket(uri);

        function message(data) {
            const line = document.createElement('p');
            line.innerText = data;
            chat.appendChild(line);
        }

        ws.onopen = function() {
            chat.innerHTML = '<p><em>Connected!</em></p>';
        };

        ws.onmessage = function(msg) {
            message(msg.data);
        };

        ws.onclose = function() {
            chat.getElementsByTagName('em')[0].innerText = 'Disconnected!';
        };

        send.onclick = function() {
            const msg = text.value;
            ws.send(msg);
            text.value = '';

            message('<You>: ' + msg);
        };
        </script>
    </body>
</html>
"#;