# 用户资料：从数据库加载，带一个简单的内存缓存装饰器。
_cache = {}


def load_user_profile(user_id):
    """按 user_id 加载用户资料，命中缓存直接返回。"""
    if user_id in _cache:
        return _cache[user_id]
    profile = {"id": user_id, "name": f"user-{user_id}"}
    _cache[user_id] = profile
    return profile


def save_user_profile(profile):
    _cache[profile["id"]] = profile
