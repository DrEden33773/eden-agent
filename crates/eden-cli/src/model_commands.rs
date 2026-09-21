//! Scriptable model selection and key management without a terminal login owner.
use crate::cli::{AuthAction, Cli, Family, ModelAction};
use eden_agent::{Session, SessionOptions};
use eden_protocol::models::{AuthRequest, CatalogRequest, ModelSelection};
use serde_json::{Value, json};
use std::{error::Error, io::Read};

async fn settled(session: &Session, run: u64) -> Result<Value, Box<dyn Error>> {
    Ok(session.wait(run).await?.into_result()?)
}
/// Commands always close the session, including when an operation fails.
pub async fn run(cli: &Cli) -> Result<i32, Box<dyn Error>> {
    let cwd = cli.cwd.clone().unwrap_or(std::env::current_dir()?);
    let session = Session::open_with_workspace(
        crate::composition(cli)?,
        SessionOptions {
            cwd,
            history: cli.session.clone(),
        },
        crate::workspace_options(cli),
    )
    .await?;
    let result=async {
        let value=match cli.family.as_ref().ok_or("missing operation")? {
            Family::Models{action}=>match action {
                ModelAction::List=>json!(session.models().await?),
                ModelAction::Current=>json!(session.model_selection().await?),
                ModelAction::Refresh=>settled(&session,session.catalog(CatalogRequest::Refresh)?).await?,
                ModelAction::Source{url}=>settled(&session,session.catalog(CatalogRequest::SetSource{url:url.clone()})?).await?,
                ModelAction::Select{provider,model,thinking}=>{
                    if cli.session.is_none(){return Err("models select requires --session; use models default for a global default".into());}
                    settled(&session,session.select_model(ModelSelection{provider:provider.clone(),model:model.clone(),thinking:thinking.clone()})?).await?
                },
                ModelAction::Default{provider,model,thinking}=>settled(&session,session.catalog(CatalogRequest::SetDefault{selection:ModelSelection{provider:provider.clone(),model:model.clone(),thinking:thinking.clone()}})?).await?,
                ModelAction::Cycle=>{
                    if cli.session.is_none(){return Err("models cycle requires --session".into());}
                    let catalog=session.models().await?;
                    let models:Vec<_>=catalog.models.into_iter().filter(|entry|entry.status=="configured").collect();
                    if models.is_empty(){return Err("no configured models available".into());}
                    let current=session.model_selection().await?;
                    let index=current.and_then(|current|models.iter().position(|entry|entry.target.provider==current.provider&&entry.target.model==current.model)).map_or(0,|index|(index+1)%models.len());
                    let target=&models[index].target;
                    settled(&session,session.select_model(ModelSelection{provider:target.provider.clone(),model:target.model.clone(),thinking:target.thinking.requested.clone()})?).await?
                },
            },
            Family::Auth{action}=>match action {
                AuthAction::Logout{provider}=>settled(&session,session.authenticate(AuthRequest::Logout{provider:provider.clone()})?).await?,
                AuthAction::Set{provider}=>{
                    let start=settled(&session,session.authenticate(AuthRequest::Start{provider:provider.clone()})?).await?;
                    let operation_id=start["operation_id"].as_str().ok_or("missing auth operation")?.to_owned();
                    let mut api_key=String::new();
                    std::io::stdin().take(65537).read_to_string(&mut api_key)?;
                    if api_key.len()>65536{return Err("API key input too large".into());}
                    settled(&session,session.authenticate(AuthRequest::Input{operation_id,api_key:api_key.trim().into()})?).await?
                },
            },
            _=>return Err("not a model operation".into()),
        };
        println!("{}",serde_json::to_string(&value)?);
        Ok(0)
    }.await;
    let shutdown = session.shutdown().await;
    match (result, shutdown) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error.into()),
        (Ok(code), Ok(())) => Ok(code),
    }
}
