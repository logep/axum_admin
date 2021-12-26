use std::collections::HashMap;

use chrono::{Local, NaiveDateTime};
use poem::{error::BadRequest, http::StatusCode, Error, Result};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, DatabaseTransaction,
    EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, Set,
};
use sea_orm_casbin_adapter::casbin::MgmtApi;
use serde_json::json;

use crate::utils::CASBIN;

use super::super::entities::{
    prelude::{SysRole, SysRoleDept},
    sys_role, sys_role_dept,
};
use super::super::models::{
    sys_role::{AddReq, DataScopeReq, DeleteReq, EditReq, Resp, SearchReq, StatusReq},
    PageParams, RespData,
};
use super::super::service;

/// get_list 获取列表
/// page_params 分页参数
/// db 数据库连接 使用db.0
pub async fn get_sort_list(
    db: &DatabaseConnection,
    page_params: PageParams,
    search_req: SearchReq,
) -> Result<RespData> {
    let page_num = page_params.page_num.unwrap_or(1);
    let page_per_size = page_params.page_size.unwrap_or(10);
    //  生成查询条件
    let mut s = SysRole::find();

    if let Some(x) = search_req.name {
        s = s.filter(sys_role::Column::Name.eq(x));
    }

    if let Some(x) = search_req.status {
        s = s.filter(sys_role::Column::Status.eq(x));
    }
    // 获取全部数据条数
    let total = s.clone().count(db).await.map_err(BadRequest)?;
    // 分页获取数据
    let paginator = s
        .order_by_asc(sys_role::Column::ListOrder)
        .order_by_asc(sys_role::Column::Id)
        .paginate(db, page_per_size);
    let num_pages = paginator.num_pages().await.map_err(BadRequest)?;
    let list = paginator
        .fetch_page(page_num - 1)
        .await
        .map_err(BadRequest)?;
    let res = json!({

        "list": list,
        "total": total,
        "total_pages": num_pages,
        "page_num": page_num,

    });
    Ok(RespData::with_data(res))
}

pub async fn check_data_is_exist(role_name: String, db: &DatabaseConnection) -> Result<bool> {
    let s1 = SysRole::find().filter(sys_role::Column::Name.eq(role_name));

    let count1 = s1.count(db).await.map_err(BadRequest)?;
    Ok(count1 > 0)
}

/// add 添加
pub async fn add(db: &DatabaseConnection, add_req: AddReq) -> Result<RespData> {
    //  检查字典类型是否存在
    if check_data_is_exist(add_req.clone().name, db).await? {
        return Err(Error::from_string(
            "数据已存在，请检查后重试",
            StatusCode::BAD_REQUEST,
        ));
    }

    // 开启事务
    let txn = db.begin().await.map_err(BadRequest)?;
    // 添加角色数据
    let role_id = self::add_role(&txn, add_req.clone()).await?;
    // 获取组合角色权限数据
    let permissions =
        self::combine_permissions_data(db, role_id.clone(), add_req.menu_ids.clone()).await?;
    // 添加角色权限数据
    unsafe {
        let e = CASBIN.get_mut().unwrap();
        e.add_policies(permissions).await.map_err(BadRequest)?;
    }

    txn.commit().await.map_err(BadRequest)?;
    let res = json!({ "id": role_id });
    Ok(RespData::with_data(res))
}

// 组合角色数据
pub async fn combine_permissions_data(
    db: &DatabaseConnection,
    role_id: String,
    permission_ids: Vec<String>,
) -> Result<Vec<Vec<String>>> {
    // 获取全部菜单
    let menus = service::sys_menu::get_all(db).await?;
    let menu_map = menus
        .iter()
        .map(|x| (x.id.clone(), x.method.clone()))
        .collect::<HashMap<String, String>>();
    // 组装角色权限数据
    let mut permissions: Vec<Vec<String>> = Vec::new();
    for permission_id in permission_ids {
        if let Some(method) = menu_map.get(&permission_id) {
            permissions.push(vec![role_id.clone(), permission_id.clone(), method.clone()]);
        }
    }
    Ok(permissions)
}

/// 添加角色数据
pub async fn add_role(txn: &DatabaseTransaction, add_req: AddReq) -> Result<String> {
    let uid = scru128::scru128_string();
    let now: NaiveDateTime = Local::now().naive_local();
    let user = sys_role::ActiveModel {
        id: Set(uid.clone()),
        name: Set(add_req.name),
        list_order: Set(add_req.list_order),
        data_scope: Set(add_req.data_scope),
        created_at: Set(Some(now)),
        status: Set(add_req.status.unwrap_or(1)),
        remark: Set(add_req.remark.unwrap_or_else(|| "".to_string())),
        ..Default::default()
    };
    SysRole::insert(user).exec(txn).await.map_err(BadRequest)?;
    Ok(uid)
}

/// delete 完全删除
pub async fn delete(db: &DatabaseConnection, delete_req: DeleteReq) -> Result<RespData> {
    let txn = db.begin().await.map_err(BadRequest)?;
    let mut s = SysRole::delete_many();
    s = s.filter(sys_role::Column::Id.is_in(delete_req.role_ids.clone()));
    //开始删除
    let d = s.exec(db).await.map_err(BadRequest)?;
    // 删除角色权限数据 和 部门权限数据
    for it in delete_req.role_ids.clone() {
        unsafe {
            let e = CASBIN.get_mut().unwrap();
            e.remove_filtered_policy(0, vec![it.clone()])
                .await
                .map_err(BadRequest)?;
        }
    }
    SysRoleDept::delete_many()
        .filter(sys_role_dept::Column::RoleId.is_in(delete_req.role_ids.clone()))
        .exec(&txn)
        .await
        .map_err(BadRequest)?;
    // 提交事务
    txn.commit().await.map_err(BadRequest)?;
    match d.rows_affected {
        0 => Err(Error::from_string(
            "删除失败,数据不存在",
            StatusCode::BAD_REQUEST,
        )),

        i => return Ok(RespData::with_msg(&format!("成功删除{}条数据", i))),
    }
}

// edit 修改
pub async fn edit(db: &DatabaseConnection, edit_req: EditReq) -> Result<RespData> {
    //  检查字典类型是否存在
    if check_data_is_exist(edit_req.clone().name, db).await? {
        return Err(Error::from_string("数据已存在", StatusCode::BAD_REQUEST));
    }
    // 开启事务
    let txn = db.begin().await.map_err(BadRequest)?;
    // 修改数据
    let uid = edit_req.id;
    let s_s = SysRole::find_by_id(uid.clone())
        .one(&txn)
        .await
        .map_err(BadRequest)?;
    let s_r: sys_role::ActiveModel = s_s.unwrap().into();
    let now: NaiveDateTime = Local::now().naive_local();
    let act = sys_role::ActiveModel {
        name: Set(edit_req.name),
        data_scope: Set(edit_req.data_scope),
        list_order: Set(edit_req.list_order),
        status: Set(edit_req.status),
        remark: Set(edit_req.remark),
        updated_at: Set(Some(now)),
        ..s_r
    };
    // 更新 //这个两种方式一样 都要多查询一次
    act.update(&txn).await.map_err(BadRequest)?;

    // 获取组合角色权限数据
    let permissions =
        self::combine_permissions_data(db, uid.clone(), edit_req.menu_ids.clone()).await?;
    unsafe {
        let e = CASBIN.get_mut().unwrap();
        // 删除全部权限 按角色id删除
        e.remove_filtered_policy(0, vec![uid.clone()])
            .await
            .map_err(BadRequest)?;
        // 添加角色权限数据
        e.add_policies(permissions).await.map_err(BadRequest)?;
    }

    // 提交事务
    txn.commit().await.map_err(BadRequest)?;

    return Ok(RespData::with_msg(&format!("用户<{}>数据更新成功", uid)));
}

// set_status 状态修改
pub async fn set_status(db: &DatabaseConnection, status_req: StatusReq) -> Result<RespData> {
    // 开启事务
    let txn = db.begin().await.map_err(BadRequest)?;
    // 修改数据
    let uid = status_req.id;
    let s_s = SysRole::find_by_id(uid.clone())
        .one(&txn)
        .await
        .map_err(BadRequest)?;
    let s_r: sys_role::ActiveModel = s_s.unwrap().into();
    let now: NaiveDateTime = Local::now().naive_local();
    let act = sys_role::ActiveModel {
        status: Set(status_req.status),
        updated_at: Set(Some(now)),
        ..s_r
    };
    act.update(&txn).await.map_err(BadRequest)?;
    txn.commit().await.map_err(BadRequest)?;
    return Ok(RespData::with_msg(&format!("用户<{}>数据更新成功", uid)));
}

// set_status 状态修改
pub async fn set_data_scope(
    db: &DatabaseConnection,
    data_scope_req: DataScopeReq,
) -> Result<RespData> {
    // 开启事务
    let txn = db.begin().await.map_err(BadRequest)?;
    // 修改数据
    let uid = data_scope_req.id;
    let s_s = SysRole::find_by_id(uid.clone())
        .one(&txn)
        .await
        .map_err(BadRequest)?;
    let s_r: sys_role::ActiveModel = s_s.unwrap().into();
    let now: NaiveDateTime = Local::now().naive_local();
    // 更新数据权限
    let data_scope = data_scope_req.data_scope;
    let act = sys_role::ActiveModel {
        data_scope: Set(data_scope),
        updated_at: Set(Some(now)),
        ..s_r
    };
    act.update(&txn).await.map_err(BadRequest)?;
    // 当数据权限为自定义数据时，删除全部权限，重新添加部门权限
    if data_scope == 2 {
        // 删除全部部门权限
        SysRoleDept::delete_many()
            .filter(sys_role_dept::Column::RoleId.eq(uid.clone()))
            .exec(&txn)
            .await
            .map_err(BadRequest)?;
        // 添加部门权限
        let mut act_datas: Vec<sys_role_dept::ActiveModel> = Vec::new();
        for dept in data_scope_req.dept_ids {
            let act_data = sys_role_dept::ActiveModel {
                role_id: Set(uid.clone()),
                dept_id: Set(dept.clone()),
                created_at: Set(Some(now)),
            };
            act_datas.push(act_data);
        }
        //  批量添加部门权限
        SysRoleDept::insert_many(act_datas)
            .exec(&txn)
            .await
            .map_err(BadRequest)?;
    }
    txn.commit().await.map_err(BadRequest)?;
    return Ok(RespData::with_msg(&format!("用户<{}>数据更新成功", uid)));
}

/// get_user_by_id 获取用户Id获取用户   
/// db 数据库连接 使用db.0
pub async fn get_by_id(db: &DatabaseConnection, search_req: SearchReq) -> Result<Resp> {
    let mut s = SysRole::find();
    //
    if let Some(x) = search_req.id {
        s = s.filter(sys_role::Column::Id.eq(x));
    } else {
        return Err(Error::from_string("id不能为空", StatusCode::BAD_REQUEST));
    }

    let res = match s.into_model::<Resp>().one(db).await.map_err(BadRequest)? {
        Some(m) => m,
        None => return Err(Error::from_string("数据不存在", StatusCode::BAD_REQUEST)),
    };

    Ok(res)
}

/// get_all 获取全部   
/// db 数据库连接 使用db.0
pub async fn get_all(db: &DatabaseConnection) -> Result<Vec<Resp>> {
    let s = SysRole::find()
        .order_by_asc(sys_role::Column::ListOrder)
        .order_by_asc(sys_role::Column::Id)
        .into_model::<Resp>()
        .all(db)
        .await
        .map_err(BadRequest)?;
    Ok(s)
}

//  获取用户角色
pub async fn get_admin_role(user_id: &str, all_roles: Vec<Resp>) -> Result<Vec<Resp>> {
    let user_id = user_id.trim();
    let role_ids = self::get_admin_role_ids(user_id).await;
    let mut roles: Vec<Resp> = Vec::new();
    for role in all_roles {
        if role_ids.contains(&role.id) {
            roles.push(role);
        }
    }
    Ok(roles)
}

//  获取用户角色ids
pub async fn get_admin_role_ids(user_id: &str) -> Vec<String> {
    let user_id = user_id.trim();
    // 查询角色关联规则
    unsafe {
        let e = CASBIN.get().unwrap();
        let group_policy = e.get_filtered_grouping_policy(0, vec![user_id.to_string()]);
        let mut role_ids = vec![];
        if !group_policy.is_empty() {
            for p in group_policy {
                role_ids.push(p[1].clone());
            }
        }
        role_ids
    }
}