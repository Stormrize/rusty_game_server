use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chess::{Board, BoardStatus, ChessMove, Color, Piece, Square};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, net::SocketAddr, str::FromStr, sync::Arc};
use tokio::sync::Mutex;
use uuid::Uuid;

const TOKEN_HEADER: &str = "x-player-token";

#[derive(Clone)]
struct AppState {
    games: Arc<Mutex<HashMap<String, Game>>>,
}

struct Game {
    board: Board,
    white_token: String,
    black_token: Option<String>,
    last_move: Option<String>,
    revision: u64,
}

#[derive(Serialize)]
struct CreateResponse {
    game_id: String,
    token: String,
    color: &'static str,
    state: GameStateDto,
}

#[derive(Serialize)]
struct JoinResponse {
    game_id: String,
    token: String,
    color: &'static str,
    state: GameStateDto,
}

#[derive(Serialize, Clone)]
struct GameStateDto {
    game_id: String,
    your_color: String,
    turn: String,
    status: String,
    fen: String,
    last_move: Option<String>,
    revision: u64,
    opponent_joined: bool,
}

#[derive(Deserialize)]
struct MoveRequest {
    from: String,
    to: String,
    #[serde(default)]
    promotion: Option<String>,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    name: &'static str,
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    message: String,
}

struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }
    fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", "Партия не найдена")
    }
    fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Нужен корректный x-player-token",
        )
    }
    fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: self.code.to_owned(),
                message: self.message,
            }),
        )
            .into_response()
    }
}

impl Game {
    fn new() -> Self {
        Self {
            board: Board::default(),
            white_token: new_token(),
            black_token: None,
            last_move: None,
            revision: 0,
        }
    }

    fn color_for_token(&self, token: &str) -> Option<Color> {
        if constant_time_eq(&self.white_token, token) {
            Some(Color::White)
        } else if self
            .black_token
            .as_deref()
            .is_some_and(|t| constant_time_eq(t, token))
        {
            Some(Color::Black)
        } else {
            None
        }
    }

    fn dto(&self, game_id: &str, color: Color) -> GameStateDto {
        let status = match self.board.status() {
            BoardStatus::Ongoing => {
                if self.black_token.is_some() {
                    "playing"
                } else {
                    "waiting"
                }
            }
            BoardStatus::Checkmate => "checkmate",
            BoardStatus::Stalemate => "stalemate",
        };
        GameStateDto {
            game_id: game_id.to_owned(),
            your_color: color_name(color).to_owned(),
            turn: color_name(self.board.side_to_move()).to_owned(),
            status: status.to_owned(),
            fen: self.board.to_string(),
            last_move: self.last_move.clone(),
            revision: self.revision,
            opponent_joined: self.black_token.is_some(),
        }
    }
}

fn new_token() -> String {
    Uuid::new_v4().simple().to_string()
}

fn color_name(color: Color) -> &'static str {
    match color {
        Color::White => "white",
        Color::Black => "black",
    }
}

// Avoid an early-exit string comparison for bearer tokens.
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.as_bytes()
        .iter()
        .zip(b.as_bytes())
        .fold(0_u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

fn token_from_headers(headers: &HeaderMap) -> Result<&str, ApiError> {
    headers
        .get(TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .ok_or_else(ApiError::unauthorized)
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        name: "esp32-chess-server",
    })
}

async fn create_game(State(state): State<AppState>) -> (StatusCode, Json<CreateResponse>) {
    let game_id = Uuid::new_v4().simple().to_string();
    let game = Game::new();
    let token = game.white_token.clone();
    let dto = game.dto(&game_id, Color::White);
    state.games.lock().await.insert(game_id.clone(), game);
    (
        StatusCode::CREATED,
        Json(CreateResponse {
            game_id,
            token,
            color: "white",
            state: dto,
        }),
    )
}

async fn join_game(
    State(state): State<AppState>,
    Path(game_id): Path<String>,
) -> Result<Json<JoinResponse>, ApiError> {
    let mut games = state.games.lock().await;
    let game = games.get_mut(&game_id).ok_or_else(ApiError::not_found)?;
    if game.black_token.is_some() {
        return Err(ApiError::conflict("В партии уже есть второй игрок"));
    }
    if game.board.status() != BoardStatus::Ongoing {
        return Err(ApiError::conflict("Эта партия уже завершена"));
    }
    let token = new_token();
    game.black_token = Some(token.clone());
    let dto = game.dto(&game_id, Color::Black);
    Ok(Json(JoinResponse {
        game_id,
        token,
        color: "black",
        state: dto,
    }))
}

async fn get_game(
    State(state): State<AppState>,
    Path(game_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<GameStateDto>, ApiError> {
    let token = token_from_headers(&headers)?;
    let games = state.games.lock().await;
    let game = games.get(&game_id).ok_or_else(ApiError::not_found)?;
    let color = game
        .color_for_token(token)
        .ok_or_else(ApiError::unauthorized)?;
    Ok(Json(game.dto(&game_id, color)))
}

async fn make_move(
    State(state): State<AppState>,
    Path(game_id): Path<String>,
    headers: HeaderMap,
    Json(input): Json<MoveRequest>,
) -> Result<Json<GameStateDto>, ApiError> {
    let token = token_from_headers(&headers)?.to_owned();
    let mut games = state.games.lock().await;
    let game = games.get_mut(&game_id).ok_or_else(ApiError::not_found)?;
    let color = game
        .color_for_token(&token)
        .ok_or_else(ApiError::unauthorized)?;

    if game.black_token.is_none() {
        return Err(ApiError::conflict("Ожидаем подключения соперника"));
    }
    if game.board.status() != BoardStatus::Ongoing {
        return Err(ApiError::conflict("Партия уже завершена"));
    }
    if color != game.board.side_to_move() {
        return Err(ApiError::conflict("Сейчас ход соперника"));
    }

    let from = Square::from_str(&input.from.to_ascii_lowercase())
        .map_err(|_| ApiError::bad_request("Поле from должно быть клеткой, например e2"))?;
    let to = Square::from_str(&input.to.to_ascii_lowercase())
        .map_err(|_| ApiError::bad_request("Поле to должно быть клеткой, например e4"))?;
    let promotion_name = input
        .promotion
        .as_deref()
        .map(|value| value.to_ascii_lowercase());
    let promotion = match promotion_name.as_deref() {
        None | Some("") | Some("null") => None,
        Some("q") | Some("queen") => Some(Piece::Queen),
        Some("r") | Some("rook") => Some(Piece::Rook),
        Some("b") | Some("bishop") => Some(Piece::Bishop),
        Some("n") | Some("knight") => Some(Piece::Knight),
        _ => return Err(ApiError::bad_request("promotion: допустимы q, r, b или n")),
    };
    let chess_move = ChessMove::new(from, to, promotion);

    // Board::legal validates movement, captures, castling, en passant,
    // check constraints and promotion as one legal chess move.
    if !game.board.legal(chess_move) {
        return Err(ApiError::conflict("Недопустимый шахматный ход"));
    }

    let mut uci = format!("{}{}", from, to);
    if let Some(piece) = promotion {
        uci.push(match piece {
            Piece::Queen => 'q',
            Piece::Rook => 'r',
            Piece::Bishop => 'b',
            Piece::Knight => 'n',
            _ => unreachable!(),
        });
    }
    game.board = game.board.make_move_new(chess_move);
    game.last_move = Some(uci);
    game.revision += 1;
    Ok(Json(game.dto(&game_id, color)))
}

#[tokio::main]
async fn main() {
    let state = AppState {
        games: Arc::new(Mutex::new(HashMap::new())),
    };
    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/games", post(create_game))
        .route("/api/games/{id}/join", post(join_game))
        .route("/api/games/{id}", get(get_game))
        .route("/api/games/{id}/moves", post(make_move))
        .with_state(state);

    // 0.0.0.0 is required so ESP32 devices on the LAN can connect.
    let addr = std::env::var("CHESS_BIND")
        .unwrap_or_else(|_| "0.0.0.0:8787".to_owned())
        .parse::<SocketAddr>()
        .expect("CHESS_BIND must be an IP:PORT socket address");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind listener");
    println!("ESP32 chess server listening on http://{addr}");
    println!("Health endpoint: http://{addr}/api/health");
    axum::serve(listener, app).await.expect("server error");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_board_is_fen() {
        let board = Board::default();
        assert!(board.to_string().starts_with("rnbqkbnr/pppppppp/"));
    }

    #[test]
    fn tokens_are_compared_without_case_folding() {
        assert!(constant_time_eq("abc123", "abc123"));
        assert!(!constant_time_eq("abc123", "ABC123"));
        assert!(!constant_time_eq("abc", "abcd"));
    }

    #[test]
    fn chess_library_rejects_illegal_move() {
        let board = Board::default();
        let legal = ChessMove::new(Square::E2, Square::E4, None);
        let illegal = ChessMove::new(Square::E2, Square::E5, None);
        assert!(board.legal(legal));
        assert!(!board.legal(illegal));
    }
}
