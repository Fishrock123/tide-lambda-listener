//! A Tide listener for AWS Lambda execution environments.
//! ### Example
//! ```no_run
//! use tide_lambda_listener::LambdaListener;
//!
//! #[async_std::main]
//! async fn main() -> tide::http::Result<()> {
//!     let mut server = tide::new();
//!
//!     server.listen(LambdaListener::new()).await?;
//!
//!     Ok(())
//! }
//! ```

#![forbid(unsafe_code)]
#![deny(
    missing_copy_implementations,
    missing_crate_level_docs,
    missing_debug_implementations,
    missing_docs,
    nonstandard_style,
    unused_qualifications
)]

use std::convert::{TryFrom, TryInto};
use std::fmt::{self, Display, Formatter};
use std::io::{Error as StdError, ErrorKind};

use async_std::io;
use http_types::{url, Body};
use lambda_http::Context;
use lambda_runtime::Config;
use surf::Client;
use tide::listener::{ListenInfo, Listener, ToListener};
use tide::Server;
use tracing::{error, trace};

/// This represents a tide [Listener](tide::listener::Listener) connected to an AWS Lambda execution environment.
pub struct LambdaListener<State> {
    client: Client,
    config: Config,
    server: Option<Server<State>>,
    info: Option<ListenInfo>,
}

impl<State> LambdaListener<State> {
    /// Create a new `LambdaListener`.
    ///
    /// ### Example
    /// ```no_run
    /// use tide_lambda_listener::LambdaListener;
    ///
    /// #[async_std::main]
    /// async fn main() -> tide::http::Result<()> {
    ///     let mut server = tide::new();
    ///
    ///     server.listen(LambdaListener::new()).await?;
    ///
    ///     Ok(())
    /// }
    /// ```
    pub fn new() -> Self {
        let config = Config::from_env().expect("(Internally asserts)");

        let inner_client: http_client::h1::H1Client = http_client::Config::new()
            .set_timeout(None)
            .try_into()
            .unwrap();
        let mut client = Client::with_http_client(inner_client);
        client.set_base_url(
            format!("http://{}", config.endpoint)
                .parse()
                .expect("Must have a valid endpoint URL set in environment; this is a lambda bug"),
        );

        Self {
            client,
            config,
            server: None,
            info: None,
        }
    }
}

impl<State: Clone + Send + Sync + 'static> ToListener<State> for LambdaListener<State> {
    type Listener = LambdaListener<State>;

    fn to_listener(self) -> io::Result<Self::Listener> {
        Ok(self)
    }
}

// Exists for error conversion
async fn handle_poll_lambda<State: Clone + Send + Sync + 'static>(
    server: Server<State>,
    client: &Client,
    config: &Config,
) -> http_types::Result<()> {
    let mut incoming = client.get("2018-06-01/runtime/invocation/next").await?;

    let mut hyperium_headers = http::HeaderMap::new();
    for (name, values) in incoming.iter() {
        let name = format!("{}", name).into_bytes();
        let name = http::header::HeaderName::from_bytes(&name).unwrap();

        for value in values.iter() {
            let value = format!("{}", value).into_bytes();
            let value = http::header::HeaderValue::from_bytes(&value).unwrap();
            hyperium_headers.append(&name, value);
        }
    }

    let ctx: Context =
        Context::try_from(hyperium_headers).map_err(|e| StdError::new(ErrorKind::Other, e))?;
    let ctx: Context = ctx.with_config(config);
    let request_id = ctx.request_id.clone();

    let event: lambda_http::request::LambdaRequest<'_> = incoming.body_json().await?;
    let request_origin = event.request_origin();

    let hyperium_event: http::Request<lambda_http::Body> = event.into();
    let (parts, body) = hyperium_event.into_parts();
    let body = match body {
        lambda_http::Body::Empty => Body::empty(),
        lambda_http::Body::Text(text) => Body::from_string(text),
        lambda_http::Body::Binary(bytes) => Body::from_bytes(bytes),
    };

    let mut req: http_types::Request = http::Request::from_parts(parts, body).try_into()?;

    req.ext_mut().insert(ctx);
    let res: http_types::Result<http_types::Response> = server.respond(req).await;

    match res {
        Ok(res) => {
            let res: http::Response<Body> = res.try_into()?;
            let (parts, body) = res.into_parts();
            let body = match body.is_empty() {
                Some(true) => lambda_http::Body::Empty,
                _ => lambda_http::Body::Text(body.into_string().await?),
            };
            let lambda_res = lambda_http::response::LambdaResponse::from_response(
                &request_origin,
                http::Response::from_parts(parts, body),
            );

            trace!("Ok response from handler (run loop)");

            client
                .post(format!(
                    "2018-06-01/runtime/invocation/{}/response",
                    request_id
                ))
                .body(Body::from_json(&lambda_res)?)
                .await?;
        }
        Err(err) => {
            error!("{}", err); // logs the error in CloudWatch

            let diagnostic_res = lambda_runtime::Diagnostic {
                error_type: type_name_of(&err).to_owned(),
                error_message: format!("{}", err), // returns the error to the caller via Lambda API
            };

            client
                .post(format!(
                    "2018-06-01/runtime/invocation/{}/error",
                    request_id
                ))
                .header("lambda-runtime-function-error-type", "unhandled")
                .body(Body::from_json(&diagnostic_res)?)
                .await?;
        }
    }

    Ok(())
}

#[tide::utils::async_trait]
impl<State> Listener<State> for LambdaListener<State>
where
    State: Clone + Send + Sync + 'static,
{
    async fn bind(&mut self, server: Server<State>) -> io::Result<()> {
        assert!(self.server.is_none(), "`bind` should only be called once");
        self.server = Some(server);

        Ok(())
    }

    async fn accept(&mut self) -> io::Result<()> {
        let server = self
            .server
            .take()
            .expect("`Listener::bind` must be called before `Listener::accept`");

        loop {
            handle_poll_lambda(server.clone(), &self.client, &self.config)
                .await
                .expect("Runtime failure");
        }
    }

    fn info(&self) -> Vec<ListenInfo> {
        match &self.info {
            Some(info) => vec![info.clone()],
            None => vec![],
        }
    }
}

impl<State> fmt::Debug for LambdaListener<State> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("LambdaListener")
            .field(&"client", &self.client)
            .field(&"config", &self.config)
            .field(
                &"server",
                if self.server.is_some() {
                    &"Some(Server<State>)"
                } else {
                    &"None"
                },
            )
            .finish()
    }
}

impl<State> Display for LambdaListener<State> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
        // let http_fmt = |a| format!("http://{}", a);
        // match &self.listener {
        //     Some(listener) => {
        //         let addr = listener.local_addr().expect("Could not get local addr");
        //         write!(f, "{}", http_fmt(&addr))
        //     }
        //     None => match &self.addrs {
        //         Some(addrs) => {
        //             let addrs = addrs.iter().map(http_fmt).collect::<Vec<_>>().join(", ");
        //             write!(f, "{}", addrs)
        //         }
        //         None => write!(f, "Not listening. Did you forget to call `Listener::bind`?"),
        //     },
        // }
    }
}

impl<State> TryFrom<Config> for LambdaListener<State> {
    type Error = url::ParseError;

    fn try_from(config: Config) -> Result<Self, Self::Error> {
        let mut client = surf::client();
        client.set_base_url(config.endpoint.parse()?);

        Ok(Self {
            client,
            config,
            server: None,
            info: None,
        })
    }
}

fn type_name_of<T: ?Sized>(_val: &T) -> &'static str {
    std::any::type_name::<T>()
}
