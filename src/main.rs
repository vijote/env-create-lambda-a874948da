use aws_config::BehaviorVersion;
use aws_sdk_cloudformation::{types::Parameter};
use aws_sdk_cloudformation::Client as CfnClient;
use aws_sdk_dynamodb::operation::put_item::PutItemError;
use lambda_http::{run, service_fn, Error, Request, Response, IntoResponse, http::{StatusCode, Method}};
use serde::{Deserialize, Serialize};
use lambda_http::RequestPayloadExt;
use std::env;
use aws_sdk_dynamodb::{Client as DynamoClient, operation::put_item::PutItemOutput};
use aws_sdk_dynamodb::types::AttributeValue;
use std::collections::HashMap;

#[derive(Deserialize)]
struct DeployRequest {
    environment_id: String,
}

#[derive(Serialize)]
struct DeployResponse {
    message: String,
    stack_id: String,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let func = service_fn(handler);
    run(func).await?;
    Ok(())
}

async fn handler(event: Request) -> Result<impl IntoResponse, Error> {
    let path = event.uri().path();
    let method = event.method();

    if method == Method::OPTIONS {
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("allow", "POST, OPTIONS")
            .header("access-control-allow-methods", "POST, OPTIONS")
            .header("access-control-allow-headers", "content-type")
            .body("".to_string())?);
    }

    if method != Method::POST || path != "/api/create" {
        return Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header("content-type", "application/json")
            .body(serde_json::to_string(&ErrorResponse {
                error: format!("Ruta no encontrada: {} {}", method, path),
            })?)?);
    }

    let payload = match event.payload::<DeployRequest>() {
        Ok(Some(p)) => p,
        _ => {
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header("content-type", "application/json")
                .body(serde_json::to_string(&ErrorResponse {
                    error: "JSON inválido o campos faltantes (se requiere 'environment_id')".to_string(),
                })?)?);
        }
    };

    let config = aws_config::load_defaults(BehaviorVersion::latest()).await;
    let cfn_client = CfnClient::new(&config);
    let dynamo_client = DynamoClient::new(&config);

    match deploy_ephemeral_stack(&cfn_client, &dynamo_client, &payload.environment_id).await {
        Ok(stack_id) => {
            let res = DeployResponse {
                message: format!("Stack para el entorno {} disparado con éxito.", payload.environment_id),
                stack_id,
            };
            
            // Retornamos un 200 OK con el JSON correspondiente
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(serde_json::to_string(&res)?)?)
        }
        Err(err) => {
            Ok(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header("content-type", "application/json")
                .body(serde_json::to_string(&ErrorResponse {
                    error: format!("Error al crear el stack de CloudFormation: {:?}", err),
                })?)?)
        }
    }
}

async fn deploy_ephemeral_stack(client: &CfnClient, dynamo_client: &DynamoClient, environment_id: &str) -> Result<String, Error> {
    let stack_name = format!("env-{}", environment_id);
    let template_url = env::var("TEMPLATE_URL")
        .expect("La variable de entorno TEMPLATE_URL no está configurada");
    let deploy_role_arn = env::var("DEPLOY_ROLE_ARN")
        .expect("La variable de entorno DEPLOY_ROLE_ARN no está configurada");
    let github_token = env::var("GITHUB_TOKEN")
        .expect("La variable de entorno GITHUB_TOKEN no está configurada");
    let table_name = env::var("TABLE_NAME")
        .expect("La variable de entorno TABLE_NAME no está configurada");

    let rule_priority = get_rule_priority(dynamo_client, &table_name)
        .await;

    create_new_environment_record(dynamo_client, &table_name, environment_id, rule_priority)
        .await
        .expect("Error al crear el entorno");

    let parameters = vec![
        Parameter::builder().parameter_key("ProjectName").parameter_value("recipes-app").build(),
        Parameter::builder().parameter_key("Environment").parameter_value(environment_id).build(),
        Parameter::builder().parameter_key("GithubWorkspaceName").parameter_value("vijote").build(),
        
        // Aquí insertamos dinámicamente el valor obtenido de DynamoDB
        Parameter::builder().parameter_key("UsersRulePriority").parameter_value((60 - rule_priority - 10).to_string()).build(),
        Parameter::builder().parameter_key("RecipesRulePriority").parameter_value((60 - rule_priority).to_string()).build(),

        Parameter::builder().parameter_key("HostMFRepositoryName").parameter_value("host-mf-a874948da").build(),
        Parameter::builder().parameter_key("RecipesMFRepositoryName").parameter_value("recipes-mf-a874948da").build(),
        Parameter::builder().parameter_key("RecipesMSRepositoryName").parameter_value("recipes-ms-a874948da").build(),
        Parameter::builder().parameter_key("UsersMSRepositoryName").parameter_value("users-ms-a874948da").build(),
        
        Parameter::builder().parameter_key("GithubToken").parameter_value(&github_token).build(),
    ];

    let response = client
        .create_stack()
        .stack_name(stack_name)
        .template_url(template_url)
        .set_parameters(Some(parameters))
        .role_arn(deploy_role_arn)
        .capabilities(aws_sdk_cloudformation::types::Capability::CapabilityIam)
        .capabilities(aws_sdk_cloudformation::types::Capability::CapabilityNamedIam)
        .send()
        .await?;

    let stack_id = response.stack_id().unwrap_or("unknown").to_string();
    Ok(stack_id)
}

pub async fn get_absolute_highest_priority(dynamo_client: &DynamoClient, table_name: &str) -> Result<i32, Box<dyn std::error::Error>> {
    let response = dynamo_client
        .scan()
        .table_name(table_name)
        .send()
        .await?;

    // Extraer, parsear y encontrar el valor máximo de forma funcional
    let highest_priority = response
        .items
        .unwrap_or_default()
        .iter()
        .filter_map(|item| item.get("highest_rule_priority"))
        .filter_map(|attr| match attr {
            aws_sdk_dynamodb::types::AttributeValue::N(val) => val.parse::<i32>().ok(),
            _ => None,
        })
        .max()           // Encuentra el número más grande de la colección
        .unwrap_or(20);   // Si la tabla está vacía, por defecto retorna 20

    Ok(highest_priority)
}

async fn get_rule_priority(
    dynamo_client: &DynamoClient,
    table_name: &str
) -> i32 {
    let highest_priority = get_absolute_highest_priority(dynamo_client, table_name).await;
    let new_priority = 60 - highest_priority.unwrap();

    new_priority
}

async fn create_new_environment_record(
    dynamo_client: &DynamoClient,
    table_name: &str,
    environment_id: &str,
    rule_priority: i32,
) -> Result<PutItemOutput, aws_smithy_runtime_api::client::result::SdkError<PutItemError, aws_smithy_runtime_api::http::Response>> {
    let mut new_item = HashMap::new();
    new_item.insert("environment".to_string(), AttributeValue::S(environment_id.to_string()));
    new_item.insert("highest_rule_priority".to_string(), AttributeValue::N(rule_priority.to_string()));
    new_item.insert("state".to_string(), AttributeValue::S("unused".to_string()));

    dynamo_client
        .put_item()
        .table_name(table_name)
        .set_item(Some(new_item))
        .send()
        .await
}