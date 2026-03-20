use crate::{
    error::Error,
    services::{get_base_href, request_delete, request_get, request_post, request_put},
};
use shared::{
    model::{RbacGroupDto, WebUiUserDto},
    utils::{concat_path, concat_path_leading_slash},
};

#[derive(Debug, Clone, serde::Serialize)]
pub struct CreateUserRequest {
    pub username: String,
    pub password: String,
    pub groups: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UpdateUserRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    pub groups: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CreateGroupRequest {
    pub name: String,
    pub permissions: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PermissionInfo {
    pub name: String,
    pub reserved: bool,
}

pub struct RbacService {
    rbac_path: String,
}

impl RbacService {
    pub fn new() -> Self {
        let base_href = get_base_href();
        Self { rbac_path: concat_path_leading_slash(&base_href, "api/v1/rbac") }
    }

    // Users
    pub async fn list_users(&self) -> Result<Option<Vec<WebUiUserDto>>, Error> {
        request_get(&concat_path(&self.rbac_path, "users"), None, None).await
    }

    pub async fn create_user(&self, req: CreateUserRequest) -> Result<(), Error> {
        request_post::<CreateUserRequest, ()>(&concat_path(&self.rbac_path, "users"), req, None, None).await?;
        Ok(())
    }

    pub async fn update_user(&self, username: &str, req: UpdateUserRequest) -> Result<(), Error> {
        request_put::<UpdateUserRequest, ()>(
            &concat_path(&self.rbac_path, &format!("users/{username}")),
            req,
            None,
            None,
        )
        .await?;
        Ok(())
    }

    pub async fn delete_user(&self, username: &str) -> Result<(), Error> {
        request_delete::<()>(&concat_path(&self.rbac_path, &format!("users/{username}")), None, None).await?;
        Ok(())
    }

    // Groups
    pub async fn list_groups(&self) -> Result<Option<Vec<RbacGroupDto>>, Error> {
        request_get(&concat_path(&self.rbac_path, "groups"), None, None).await
    }

    pub async fn create_group(&self, req: CreateGroupRequest) -> Result<(), Error> {
        request_post::<CreateGroupRequest, ()>(&concat_path(&self.rbac_path, "groups"), req, None, None).await?;
        Ok(())
    }

    pub async fn update_group(&self, name: &str, req: CreateGroupRequest) -> Result<(), Error> {
        request_put::<CreateGroupRequest, ()>(
            &concat_path(&self.rbac_path, &format!("groups/{name}")),
            req,
            None,
            None,
        )
        .await?;
        Ok(())
    }

    pub async fn delete_group(&self, name: &str) -> Result<(), Error> {
        request_delete::<()>(&concat_path(&self.rbac_path, &format!("groups/{name}")), None, None).await?;
        Ok(())
    }

    // Permissions
    pub async fn list_permissions(&self) -> Result<Option<Vec<PermissionInfo>>, Error> {
        request_get(&concat_path(&self.rbac_path, "permissions"), None, None).await
    }
}

impl Default for RbacService {
    fn default() -> Self { Self::new() }
}
