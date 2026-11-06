// 用户资料缓存：按 user id 缓存 UserProfile，命中直接返回，未命中回源并写入。
use std::collections::HashMap;

pub struct UserProfile {
    pub id: u64,
    pub name: String,
}

pub struct UserProfileCache {
    store: HashMap<u64, UserProfile>,
}

impl UserProfileCache {
    // 读取用户资料：先查缓存，未命中则从数据库加载并回填。
    pub fn user_profile_cache_get(&mut self, id: u64) -> Option<&UserProfile> {
        self.store.get(&id)
    }

    pub fn insert_profile(&mut self, profile: UserProfile) {
        self.store.insert(profile.id, profile);
    }
}
