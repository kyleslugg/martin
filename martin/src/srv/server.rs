use std::string::ToString;
use std::time::Duration;

use actix_cors::Cors;
use actix_http::ContentEncoding;
use actix_web::dev::Server;
use actix_web::error::{ErrorBadRequest, ErrorInternalServerError, ErrorNotFound};
use actix_web::http::header::{
    AcceptEncoding, ContentType, Encoding as HeaderEnc, Preference, CACHE_CONTROL, CONTENT_ENCODING,
};
use actix_web::http::Uri;
use actix_web::middleware::TrailingSlash;
use actix_web::web::{Data, Path, Query};
use actix_web::{
    middleware, route, web, App, HttpMessage, HttpRequest, HttpResponse, HttpServer, Responder,
    Result as ActixResult,
};
use futures::future::try_join_all;
use itertools::Itertools as _;
use log::error;
use martin_tile_utils::{Encoding, Format, TileInfo};
use serde::{Deserialize, Serialize};
use tilejson::{tilejson, TileJSON};

use crate::config::ServerState;
use crate::fonts::{FontCatalog, FontError, FontSources};
use crate::source::{Source, TileCatalog, TileSources, UrlQuery};
use crate::sprites::{SpriteCatalog, SpriteError, SpriteSources};
use crate::srv::config::{SrvConfig, KEEP_ALIVE_DEFAULT, LISTEN_ADDRESSES_DEFAULT};
use crate::utils::{decode_brotli, decode_gzip, encode_brotli, encode_gzip};
use crate::MartinError::BindingError;
use crate::{MartinResult, Tile, TileCoord};

/// List of keywords that cannot be used as source IDs. Some of these are reserved for future use.
/// Reserved keywords must never end in a "dot number" (e.g. ".1").
/// This list is documented in the `docs/src/using.md` file, which should be kept in sync.
pub const RESERVED_KEYWORDS: &[&str] = &[
    "_", "catalog", "config", "font", "health", "help", "index", "manifest", "metrics", "refresh",
    "reload", "sprite", "status",
];

static SUPPORTED_ENCODINGS: &[HeaderEnc] = &[
    HeaderEnc::brotli(),
    HeaderEnc::gzip(),
    HeaderEnc::identity(),
];

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Catalog {
    pub tiles: TileCatalog,
    pub sprites: SpriteCatalog,
    pub fonts: FontCatalog,
}

impl Catalog {
    pub fn new(state: &ServerState) -> MartinResult<Self> {
        Ok(Self {
            tiles: state.tiles.get_catalog(),
            sprites: state.sprites.get_catalog()?,
            fonts: state.fonts.get_catalog(),
        })
    }
}

#[derive(Deserialize)]
struct TileJsonRequest {
    source_ids: String,
}

#[derive(Deserialize, Clone)]
pub struct TileRequest {
    source_ids: String,
    z: u8,
    x: u32,
    y: u32,
}

pub fn map_internal_error<T: std::fmt::Display>(e: T) -> actix_web::Error {
    error!("{e}");
    ErrorInternalServerError(e.to_string())
}

pub fn map_sprite_error(e: SpriteError) -> actix_web::Error {
    use SpriteError::SpriteNotFound;
    match e {
        SpriteNotFound(_) => ErrorNotFound(e.to_string()),
        _ => map_internal_error(e),
    }
}

pub fn map_font_error(e: FontError) -> actix_web::Error {
    #[allow(clippy::enum_glob_use)]
    use FontError::*;
    match e {
        FontNotFound(_) => ErrorNotFound(e.to_string()),
        InvalidFontRangeStartEnd(_, _)
        | InvalidFontRangeStart(_)
        | InvalidFontRangeEnd(_)
        | InvalidFontRange(_, _) => ErrorBadRequest(e.to_string()),
        _ => map_internal_error(e),
    }
}

/// Root path will eventually have a web front. For now, just a stub.
#[route("/", method = "GET", method = "HEAD")]
#[allow(clippy::unused_async)]
async fn get_index() -> &'static str {
    // todo: once this becomes more substantial, add wrap = "middleware::Compress::default()"
    "Martin server is running. Eventually this will be a nice web front.\n\n\
    A list of all available sources is at /catalog\n\n\
    See documentation https://github.com/maplibre/martin"
}

/// Return 200 OK if healthy. Used for readiness and liveness probes.
#[route("/health", method = "GET", method = "HEAD")]
#[allow(clippy::unused_async)]
async fn get_health() -> impl Responder {
    HttpResponse::Ok()
        .insert_header((CACHE_CONTROL, "no-cache"))
        .message_body("OK")
}

#[route(
    "/catalog",
    method = "GET",
    method = "HEAD",
    wrap = "middleware::Compress::default()"
)]
#[allow(clippy::unused_async)]
async fn get_catalog(catalog: Data<Catalog>) -> impl Responder {
    HttpResponse::Ok().json(catalog)
}

#[route("/sprite/{source_ids}.png", method = "GET", method = "HEAD")]
async fn get_sprite_png(
    path: Path<TileJsonRequest>,
    sprites: Data<SpriteSources>,
) -> ActixResult<HttpResponse> {
    let sheet = sprites
        .get_sprites(&path.source_ids)
        .await
        .map_err(map_sprite_error)?;
    Ok(HttpResponse::Ok()
        .content_type(ContentType::png())
        .body(sheet.encode_png().map_err(map_internal_error)?))
}

#[route(
    "/sprite/{source_ids}.json",
    method = "GET",
    method = "HEAD",
    wrap = "middleware::Compress::default()"
)]
async fn get_sprite_json(
    path: Path<TileJsonRequest>,
    sprites: Data<SpriteSources>,
) -> ActixResult<HttpResponse> {
    let sheet = sprites
        .get_sprites(&path.source_ids)
        .await
        .map_err(map_sprite_error)?;
    Ok(HttpResponse::Ok().json(sheet.get_index()))
}

#[derive(Deserialize, Debug)]
struct FontRequest {
    fontstack: String,
    start: u32,
    end: u32,
}

#[route(
    "/font/{fontstack}/{start}-{end}",
    method = "GET",
    wrap = "middleware::Compress::default()"
)]
#[allow(clippy::unused_async)]
async fn get_font(path: Path<FontRequest>, fonts: Data<FontSources>) -> ActixResult<HttpResponse> {
    let data = fonts
        .get_font_range(&path.fontstack, path.start, path.end)
        .map_err(map_font_error)?;
    Ok(HttpResponse::Ok()
        .content_type("application/x-protobuf")
        .body(data))
}

#[route(
    "/{source_ids}",
    method = "GET",
    method = "HEAD",
    wrap = "middleware::Compress::default()"
)]
#[allow(clippy::unused_async)]
async fn git_source_info(
    req: HttpRequest,
    path: Path<TileJsonRequest>,
    sources: Data<TileSources>,
) -> ActixResult<HttpResponse> {
    let sources = sources.get_sources(&path.source_ids, None)?.0;

    // Get `X-REWRITE-URL` header value, and extract its `path` component.
    // If the header is not present or cannot be parsed as a URL, return the request path.
    let tiles_path = req
        .headers()
        .get("x-rewrite-url")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<Uri>().ok())
        .map_or_else(|| req.path().to_owned(), |v| v.path().to_owned());

    let query_string = req.query_string();
    let path_and_query = if query_string.is_empty() {
        format!("{tiles_path}/{{z}}/{{x}}/{{y}}")
    } else {
        format!("{tiles_path}/{{z}}/{{x}}/{{y}}?{query_string}")
    };

    // Construct a tiles URL from the request info, including the query string if present.
    let info = req.connection_info();
    let tiles_url = Uri::builder()
        .scheme(info.scheme())
        .authority(info.host())
        .path_and_query(path_and_query)
        .build()
        .map(|tiles_url| tiles_url.to_string())
        .map_err(|e| ErrorBadRequest(format!("Can't build tiles URL: {e}")))?;

    Ok(HttpResponse::Ok().json(merge_tilejson(&sources, tiles_url)))
}

#[must_use]
pub fn merge_tilejson(sources: &[&dyn Source], tiles_url: String) -> TileJSON {
    if sources.len() == 1 {
        let mut tj = sources[0].get_tilejson().clone();
        tj.tiles = vec![tiles_url];
        return tj;
    }

    let mut attributions = vec![];
    let mut descriptions = vec![];
    let mut names = vec![];
    let mut result = tilejson! {
        tiles: vec![tiles_url],
    };

    for src in sources {
        let tj = src.get_tilejson();

        if let Some(vector_layers) = &tj.vector_layers {
            if let Some(ref mut a) = result.vector_layers {
                a.extend(vector_layers.iter().cloned());
            } else {
                result.vector_layers = Some(vector_layers.clone());
            }
        }

        if let Some(v) = &tj.attribution {
            if !attributions.contains(&v) {
                attributions.push(v);
            }
        }

        if let Some(bounds) = tj.bounds {
            if let Some(a) = result.bounds {
                result.bounds = Some(a + bounds);
            } else {
                result.bounds = tj.bounds;
            }
        }

        if result.center.is_none() {
            // Use first found center. Averaging multiple centers might create a center in the middle of nowhere.
            result.center = tj.center;
        }

        if let Some(v) = &tj.description {
            if !descriptions.contains(&v) {
                descriptions.push(v);
            }
        }

        if let Some(maxzoom) = tj.maxzoom {
            if let Some(a) = result.maxzoom {
                if a < maxzoom {
                    result.maxzoom = tj.maxzoom;
                }
            } else {
                result.maxzoom = tj.maxzoom;
            }
        }

        if let Some(minzoom) = tj.minzoom {
            if let Some(a) = result.minzoom {
                if a > minzoom {
                    result.minzoom = tj.minzoom;
                }
            } else {
                result.minzoom = tj.minzoom;
            }
        }

        if let Some(name) = &tj.name {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }

    if !attributions.is_empty() {
        result.attribution = Some(attributions.into_iter().join("\n"));
    }

    if !descriptions.is_empty() {
        result.description = Some(descriptions.into_iter().join("\n"));
    }

    if !names.is_empty() {
        result.name = Some(names.into_iter().join(","));
    }

    result
}

#[route("/{source_ids}/{z}/{x}/{y}", method = "GET", method = "HEAD")]
async fn get_tile(
    req: HttpRequest,
    path: Path<TileRequest>,
    sources: Data<TileSources>,
) -> ActixResult<HttpResponse> {
    let xyz = TileCoord {
        z: path.z,
        x: path.x,
        y: path.y,
    };

    let source_ids = &path.source_ids;
    let query = req.query_string();
    let encodings = req.get_header::<AcceptEncoding>();

    get_tile_response(sources.as_ref(), xyz, source_ids, query, encodings).await
}

pub async fn get_tile_response(
    sources: &TileSources,
    xyz: TileCoord,
    source_ids: &str,
    query: &str,
    encodings: Option<AcceptEncoding>,
) -> ActixResult<HttpResponse> {
    let (sources, use_url_query, info) = sources.get_sources(source_ids, Some(xyz.z))?;

    let query = use_url_query.then_some(query);
    let tile = get_tile_content(sources.as_slice(), info, &xyz, query, encodings.as_ref()).await?;

    Ok(if tile.data.is_empty() {
        HttpResponse::NoContent().finish()
    } else {
        let mut response = HttpResponse::Ok();
        response.content_type(tile.info.format.content_type());
        if let Some(val) = tile.info.encoding.content_encoding() {
            response.insert_header((CONTENT_ENCODING, val));
        }
        response.body(tile.data)
    })
}

pub async fn get_tile_content(
    sources: &[&dyn Source],
    info: TileInfo,
    xyz: &TileCoord,
    query: Option<&str>,
    encodings: Option<&AcceptEncoding>,
) -> ActixResult<Tile> {
    if sources.is_empty() {
        return Err(ErrorNotFound("No valid sources found"));
    }
    let query = match query {
        Some(v) if !v.is_empty() => Some(Query::<UrlQuery>::from_query(v)?.into_inner()),
        _ => None,
    };

    let mut tiles = try_join_all(sources.iter().map(|s| s.get_tile(xyz, &query)))
        .await
        .map_err(map_internal_error)?;

    // Make sure tiles can be concatenated, or if not, that there is only one non-empty tile for each zoom level
    // TODO: can zlib, brotli, or zstd be concatenated?
    // TODO: implement decompression step for other concatenate-able formats
    let can_join = info.format == Format::Mvt
        && (info.encoding == Encoding::Uncompressed || info.encoding == Encoding::Gzip);
    let layer_count = tiles.iter().filter(|v| !v.is_empty()).count();
    if !can_join && layer_count > 1 {
        return Err(ErrorBadRequest(format!(
            "Can't merge {info} tiles. Make sure there is only one non-empty tile source at zoom level {}",
            xyz.z
        )))?;
    }

    // Minor optimization to prevent concatenation if there are less than 2 tiles
    let data = match layer_count {
        1 => tiles.swap_remove(0),
        0 => return Ok(Tile::new(Vec::new(), info)),
        _ => tiles.concat(),
    };

    // decide if (re-)encoding of the tile data is needed, and recompress if so
    let tile = recompress(Tile::new(data, info), encodings)?;

    Ok(tile)
}

fn recompress(mut tile: Tile, accept_enc: Option<&AcceptEncoding>) -> ActixResult<Tile> {
    if let Some(accept_enc) = accept_enc {
        if tile.info.encoding.is_encoded() {
            // already compressed, see if we can send it as is, or need to re-compress
            if !accept_enc.iter().any(|e| {
                if let Preference::Specific(HeaderEnc::Known(enc)) = e.item {
                    to_encoding(enc) == Some(tile.info.encoding)
                } else {
                    false
                }
            }) {
                // need to re-compress the tile - uncompress it first
                tile = decode(tile)?;
            }
        }
        if tile.info.encoding == Encoding::Uncompressed {
            // only apply compression if the content supports it
            if let Some(HeaderEnc::Known(enc)) = accept_enc.negotiate(SUPPORTED_ENCODINGS.iter()) {
                // (re-)compress the tile into the preferred encoding
                tile = encode(tile, enc)?;
            }
        }
        Ok(tile)
    } else {
        // no accepted-encoding header, decode the tile if compressed
        decode(tile)
    }
}

fn encode(tile: Tile, enc: ContentEncoding) -> ActixResult<Tile> {
    Ok(match enc {
        ContentEncoding::Brotli => Tile::new(
            encode_brotli(&tile.data)?,
            tile.info.encoding(Encoding::Brotli),
        ),
        ContentEncoding::Gzip => {
            Tile::new(encode_gzip(&tile.data)?, tile.info.encoding(Encoding::Gzip))
        }
        _ => tile,
    })
}

fn decode(tile: Tile) -> ActixResult<Tile> {
    let info = tile.info;
    Ok(if info.encoding.is_encoded() {
        match info.encoding {
            Encoding::Gzip => Tile::new(
                decode_gzip(&tile.data)?,
                info.encoding(Encoding::Uncompressed),
            ),
            Encoding::Brotli => Tile::new(
                decode_brotli(&tile.data)?,
                info.encoding(Encoding::Uncompressed),
            ),
            _ => Err(ErrorBadRequest(format!(
                "Tile is is stored as {info}, but the client does not accept this encoding"
            )))?,
        }
    } else {
        tile
    })
}

fn to_encoding(val: ContentEncoding) -> Option<Encoding> {
    Some(match val {
        ContentEncoding::Identity => Encoding::Uncompressed,
        ContentEncoding::Gzip => Encoding::Gzip,
        ContentEncoding::Brotli => Encoding::Brotli,
        // TODO: Deflate => Encoding::Zstd or Encoding::Zlib ?
        _ => None?,
    })
}

pub fn router(cfg: &mut web::ServiceConfig) {
    cfg.service(get_health)
        .service(get_index)
        .service(get_catalog)
        .service(git_source_info)
        .service(get_tile)
        .service(get_sprite_json)
        .service(get_sprite_png)
        .service(get_font);
}

/// Create a new initialized Actix `App` instance together with the listening address.
pub fn new_server(config: SrvConfig, state: ServerState) -> MartinResult<(Server, String)> {
    let catalog = Catalog::new(&state)?;
    let keep_alive = Duration::from_secs(config.keep_alive.unwrap_or(KEEP_ALIVE_DEFAULT));
    let worker_processes = config.worker_processes.unwrap_or_else(num_cpus::get);
    let listen_addresses = config
        .listen_addresses
        .unwrap_or_else(|| LISTEN_ADDRESSES_DEFAULT.to_owned());

    let server = HttpServer::new(move || {
        let cors_middleware = Cors::default()
            .allow_any_origin()
            .allowed_methods(vec!["GET"]);

        App::new()
            .app_data(Data::new(state.tiles.clone()))
            .app_data(Data::new(state.sprites.clone()))
            .app_data(Data::new(state.fonts.clone()))
            .app_data(Data::new(catalog.clone()))
            .wrap(cors_middleware)
            .wrap(middleware::NormalizePath::new(TrailingSlash::MergeOnly))
            .wrap(middleware::Logger::default())
            .configure(router)
    })
    .bind(listen_addresses.clone())
    .map_err(|e| BindingError(e, listen_addresses.clone()))?
    .keep_alive(keep_alive)
    .shutdown_timeout(0)
    .workers(worker_processes)
    .run();

    Ok((server, listen_addresses))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use async_trait::async_trait;
    use tilejson::{tilejson, Bounds, VectorLayer};

    use super::*;
    use crate::source::{Source, TileData};

    #[derive(Debug, Clone)]
    struct TestSource {
        tj: TileJSON,
    }

    #[async_trait]
    impl Source for TestSource {
        fn get_id(&self) -> &str {
            "id"
        }

        fn get_tilejson(&self) -> &TileJSON {
            &self.tj
        }

        fn get_tile_info(&self) -> TileInfo {
            unimplemented!()
        }

        fn clone_source(&self) -> Box<dyn Source> {
            unimplemented!()
        }

        async fn get_tile(
            &self,
            _xyz: &TileCoord,
            _url_query: &Option<UrlQuery>,
        ) -> MartinResult<TileData> {
            unimplemented!()
        }
    }

    #[test]
    fn test_merge_tilejson() {
        let url = "http://localhost:8888/foo/{z}/{x}/{y}".to_string();
        let src1 = TestSource {
            tj: tilejson! {
                tiles: vec![],
                name: "layer1".to_string(),
                minzoom: 5,
                maxzoom: 10,
                bounds: Bounds::new(-10.0, -20.0, 10.0, 20.0),
                vector_layers: vec![
                    VectorLayer::new("layer1".to_string(),
                    BTreeMap::from([
                        ("a".to_string(), "x1".to_string()),
                    ]))
                ],
            },
        };
        let tj = merge_tilejson(&[&src1], url.clone());
        assert_eq!(
            TileJSON {
                tiles: vec![url.clone()],
                ..src1.tj.clone()
            },
            tj
        );

        let src2 = TestSource {
            tj: tilejson! {
                tiles: vec![],
                name: "layer2".to_string(),
                minzoom: 7,
                maxzoom: 12,
                bounds: Bounds::new(-20.0, -5.0, 5.0, 50.0),
                vector_layers: vec![
                    VectorLayer::new("layer2".to_string(),
                    BTreeMap::from([
                        ("b".to_string(), "x2".to_string()),
                    ]))
                ],
            },
        };

        let tj = merge_tilejson(&[&src1, &src2], url.clone());
        assert_eq!(tj.tiles, vec![url]);
        assert_eq!(tj.name, Some("layer1,layer2".to_string()));
        assert_eq!(tj.minzoom, Some(5));
        assert_eq!(tj.maxzoom, Some(12));
        assert_eq!(tj.bounds, Some(Bounds::new(-20.0, -20.0, 10.0, 50.0)));
        assert_eq!(
            tj.vector_layers,
            Some(vec![
                VectorLayer::new(
                    "layer1".to_string(),
                    BTreeMap::from([("a".to_string(), "x1".to_string())])
                ),
                VectorLayer::new(
                    "layer2".to_string(),
                    BTreeMap::from([("b".to_string(), "x2".to_string())])
                ),
            ])
        );
    }
}
