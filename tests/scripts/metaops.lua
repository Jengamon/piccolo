
local debug = false

local function dump(obj)
    if type(obj) == "table" then
        local s = "{"
        local seq_len = rawlen(obj)
        local actual_len = 0
        for k, v in pairs(obj) do actual_len = actual_len + 1 end
        if seq_len == actual_len then
            local sep = " "
            -- would use ipairs here, but that breaks with the len and index overrides
            for i = 1, seq_len do
                s = s .. sep .. dump(rawget(obj, i))
                sep = ", "
            end
        else
            local sep = " "
            for k, v in pairs(obj) do
                if type(k) ~= "string" then
                    k = "[" .. k .. "]"
                end
                s = s .. sep .. k .. " = " .. dump(v)
                sep = ", "
            end
        end
        s = s .. " }"
        return s
    elseif type(obj) == "string" then
        return '"' .. obj .. '"' -- note: no proper escaping
    else
        return tostring(obj)
    end
end

local primitives = { ["nil"] = 0, number = 0, string = 0, boolean = 0, ["function"] = 0, thread = 0 }
local function cmp_array_recurse(a, b, top)
    if top == nil and debug then
        print(dump(a))
        print(dump(b))
    end
    local a_ty = type(a)
    local b_ty = type(b)
    if a_ty ~= b_ty then
        return false
    end
    if primitives[a_ty] ~= nil then
        return a == b
    end
    if rawlen(a) ~= rawlen(b) then
      return false
    end
    for i = 1, rawlen(a) do
        if not cmp_array_recurse(rawget(a, i), rawget(b, i), false) then
            return false
        end
    end
    return true
end

do
    local function add(a, b) return { "add", a, b } end
    local function sub(a, b) return { "sub", a, b } end
    local function mul(a, b) return { "mul", a, b } end
    local function div(a, b) return { "div", a, b } end

    local mt = {
        __add = add,
        __sub = sub,
        __mul = mul,
        __div = div,
    }

    local a = { "a" }
    local b = { "b" }
    local c = { "c" }

    setmetatable(a, mt)
    setmetatable(b, mt)
    setmetatable(c, mt)

    local res

    res = a + b * c - a
    assert(cmp_array_recurse(res, { "sub", { "add", { "a" }, { "mul", { "b" }, { "c" } } }, { "a" } }))

    res = c / a
    assert(cmp_array_recurse(res, { "div", { "c" }, { "a" } }))

    res = a * a
    assert(cmp_array_recurse(res, { "mul", { "a" }, { "a" } }))
end

-- note in the __len docs:
-- "the result of the call (always adjusted to one value) is the result of the operation"
do
    local mt = {
        __len = function(val) return 5, 6, 7 end,
    }

    local a = setmetatable({ "a" }, mt)
    local i, j, k = #a
    assert(i == 5 and j == nil and k == nil)
    assert(select("#", #a) == 1)
end

do
    local cursed_mt = {}
    cursed_mt["__add"] = function(a, b) return setmetatable({ "add", a, b }, cursed_mt) end
    cursed_mt["__sub"] = function(a, b) return setmetatable({ "sub", a, b }, cursed_mt) end
    cursed_mt["__mul"] = function(a, b) return setmetatable({ "mul", a, b }, cursed_mt) end
    cursed_mt["__div"] = function(a, b) return setmetatable({ "div", a, b }, cursed_mt) end

    cursed_mt["__mod"] = function(a, b) return setmetatable({ "mod", a, b }, cursed_mt) end
    cursed_mt["__pow"] = function(a, b) return setmetatable({ "pow", a, b }, cursed_mt) end
    cursed_mt["__unm"] = function(val) return setmetatable({ "unm", val }, cursed_mt) end
    cursed_mt["__idiv"] = function(a, b) return setmetatable({ "idiv", a, b }, cursed_mt) end

    cursed_mt["__band"] = function(a, b) return setmetatable({ "band", a, b }, cursed_mt) end
    cursed_mt["__bor"] = function(a, b) return setmetatable({ "bor", a, b }, cursed_mt) end
    cursed_mt["__bxor"] = function(a, b) return setmetatable({ "bxor", a, b }, cursed_mt) end
    cursed_mt["__bnot"] = function(val) return setmetatable({ "bnot", val }, cursed_mt) end
    cursed_mt["__shl"] = function(a, b) return setmetatable({ "shl", a, b }, cursed_mt) end
    cursed_mt["__shr"] = function(a, b) return setmetatable({ "shr", a, b }, cursed_mt) end

    cursed_mt["__len"] = function(val) return setmetatable({ "len", val }, cursed_mt) end
    cursed_mt["__index"] = function(a, b) return setmetatable({ "index", a, b }, cursed_mt) end
    cursed_mt["__call"] = function(this, ...) return setmetatable({ "call", this, ... }, cursed_mt) end

    -- Not tested here:
    -- cursed_mt["__newindex"] = function(a, b) end
    -- cursed_mt["__eq"] = function(a, b) return false end
    -- cursed_mt["__lt"] = function(a, b) return false end
    -- cursed_mt["__le"] = function(a, b) return false end
    -- cursed_mt["__concat"] = function(a, b) return setmetatable({ "concat", a, b }, cursed_mt) end

    local function curse(val)
        return setmetatable(val, cursed_mt)
    end

    local a = curse { "a" }
    local b = curse { "b" }

    assert(cmp_array_recurse(a + b, { "add", { "a" }, { "b" } }))
    assert(cmp_array_recurse(a - b, { "sub", { "a" }, { "b" } }))
    assert(cmp_array_recurse(a * b, { "mul", { "a" }, { "b" } }))
    assert(cmp_array_recurse(a / b, { "div", { "a" }, { "b" } }))

    assert(cmp_array_recurse(a % b, { "mod", { "a" }, { "b" } }))
    assert(cmp_array_recurse(a ^ b, { "pow", { "a" }, { "b" } }))
    assert(cmp_array_recurse(-a, { "unm", { "a" } }))
    assert(cmp_array_recurse(a // b, { "idiv", { "a" }, { "b" } }))

    assert(cmp_array_recurse(a & b, { "band", { "a" }, { "b" } }))
    assert(cmp_array_recurse(a | b, { "bor", { "a" }, { "b" } }))
    assert(cmp_array_recurse(a ~ b, { "bxor", { "a" }, { "b" } }))
    assert(cmp_array_recurse(~a, { "bnot", { "a" } }))
    assert(cmp_array_recurse(a << b, { "shl", { "a" }, { "b" } }))
    assert(cmp_array_recurse(a >> b, { "shr", { "a" }, { "b" } }))

    assert(cmp_array_recurse(#a, { "len", { "a" } }))
    assert(cmp_array_recurse(a.b, { "index", { "a" }, "b" }))
    assert(cmp_array_recurse(a(), { "call", { "a" } }))
    assert(cmp_array_recurse(a(1, 2, 3), { "call", { "a" }, 1, 2, 3 }))

end

