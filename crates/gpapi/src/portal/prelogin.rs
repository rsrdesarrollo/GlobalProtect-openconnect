use anyhow::{anyhow, bail};
use log::{info, warn};
use reqwest::{Client, StatusCode};
use roxmltree::Document;
use serde::Serialize;
use specta::Type;

use crate::{
  error::PortalError,
  gp_params::GpParams,
  utils::{base64, normalize_server, parse_gp_response, xml},
};

const REQUIRED_PARAMS: [&str; 8] = [
  "tmp",
  "clientVer",
  "clientos",
  "os-version",
  "host-id",
  "ipv6-support",
  "default-browser",
  "cas-support",
];

#[derive(Debug, Serialize, Type, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SamlPrelogin {
  region: String,
  is_gateway: bool,
  saml_request: String,
  support_default_browser: bool,
}

impl SamlPrelogin {
  pub fn region(&self) -> &str {
    &self.region
  }

  pub fn saml_request(&self) -> &str {
    &self.saml_request
  }

  pub fn support_default_browser(&self) -> bool {
    self.support_default_browser
  }
}

#[derive(Debug, Serialize, Type, Clone)]
#[serde(rename_all = "camelCase")]
pub struct StandardPrelogin {
  region: String,
  is_gateway: bool,
  auth_message: String,
  label_username: String,
  label_password: String,
}

impl StandardPrelogin {
  pub fn region(&self) -> &str {
    &self.region
  }

  pub fn auth_message(&self) -> &str {
    &self.auth_message
  }

  pub fn label_username(&self) -> &str {
    &self.label_username
  }

  pub fn label_password(&self) -> &str {
    &self.label_password
  }
}

#[derive(Debug, Serialize, Type, Clone)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Prelogin {
  Saml(SamlPrelogin),
  Standard(StandardPrelogin),
}

impl Prelogin {
  pub fn region(&self) -> &str {
    match self {
      Prelogin::Saml(saml) => saml.region(),
      Prelogin::Standard(standard) => standard.region(),
    }
  }

  pub fn is_gateway(&self) -> bool {
    match self {
      Prelogin::Saml(saml) => saml.is_gateway,
      Prelogin::Standard(standard) => standard.is_gateway,
    }
  }
}

pub async fn prelogin(portal: &str, gp_params: &GpParams) -> anyhow::Result<Prelogin> {
  let user_agent = gp_params.user_agent();
  info!("Prelogin with user_agent: {}", user_agent);

  let portal = normalize_server(portal)?;
  let is_gateway = gp_params.is_gateway();
  let path = if is_gateway { "ssl-vpn" } else { "global-protect" };
  let prelogin_url = format!("{portal}/{}/prelogin.esp", path);
  let mut params = gp_params.to_params();

  params.insert("tmp", "tmp");
  params.insert("default-browser", "1");
  params.insert("cas-support", "yes");

  params.retain(|k, _| REQUIRED_PARAMS.iter().any(|required_param| required_param == k));

  info!("Prelogin with params: {:?}", params);

  let client = Client::builder()
    .danger_accept_invalid_certs(gp_params.ignore_tls_errors())
    .user_agent(user_agent)
    .build()?;

  let res = client
    .post(&prelogin_url)
    .form(&params)
    .send()
    .await
    .map_err(|e| anyhow::anyhow!(PortalError::NetworkError(e.to_string())))?;

  let res_xml = parse_gp_response(res).await.or_else(|err| {
    if err.status == StatusCode::NOT_FOUND {
      bail!(PortalError::PreloginError("Prelogin endpoint not found".to_string()))
    }

    if err.is_status_error() {
      warn!("{err}");
      bail!("Prelogin error: {}", err.reason)
    }

    Err(anyhow!(PortalError::PreloginError(err.reason)))
  })?;

  let prelogin = parse_res_xml(&res_xml, is_gateway).map_err(|err| {
    warn!("Parse response error, response: {}", res_xml);
    PortalError::PreloginError(err.to_string())
  })?;

  Ok(prelogin)
}

fn parse_res_xml(res_xml: &str, is_gateway: bool) -> anyhow::Result<Prelogin> {
  let doc = Document::parse(res_xml)?;

  let status = xml::get_child_text(&doc, "status")
    .ok_or_else(|| anyhow::anyhow!("Prelogin response does not contain status element"))?;
  // Check the status of the prelogin response
  if status.to_uppercase() != "SUCCESS" {
    let msg = xml::get_child_text(&doc, "msg").unwrap_or(String::from("Unknown error"));
    bail!("{}", msg)
  }

  let region = xml::get_child_text(&doc, "region").unwrap_or_else(|| {
    info!("Prelogin response does not contain region element");
    String::from("Unknown")
  });

  let saml_method = xml::get_child_text(&doc, "saml-auth-method");
  let saml_request = xml::get_child_text(&doc, "saml-request");
  let saml_default_browser = xml::get_child_text(&doc, "saml-default-browser");
  // Check if the prelogin response is SAML
  if saml_method.is_some() && saml_request.is_some() {
    let saml_request = base64::decode_to_string(&saml_request.unwrap())?;
    let support_default_browser = saml_default_browser.map(|s| s.to_lowercase() == "yes").unwrap_or(false);

    let saml_prelogin = SamlPrelogin {
      region,
      is_gateway,
      saml_request,
      support_default_browser,
    };

    return Ok(Prelogin::Saml(saml_prelogin));
  }

  let label_username = xml::get_child_text(&doc, "username-label");
  let label_password = xml::get_child_text(&doc, "password-label");
  // Check if the prelogin response is standard login
  if label_username.is_some() && label_password.is_some() {
    let auth_message =
      xml::get_child_text(&doc, "authentication-message").unwrap_or(String::from("Please enter the login credentials"));
    let standard_prelogin = StandardPrelogin {
      region,
      is_gateway,
      auth_message,
      label_username: label_username.unwrap(),
      label_password: label_password.unwrap(),
    };

    Ok(Prelogin::Standard(standard_prelogin))
  } else {
    Err(anyhow!("Invalid prelogin response"))
  }
}
