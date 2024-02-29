
local p = { x = 2, y = 2 };
local x = "sssssssssssssssssssssssssssssssssssssssssssssssssss";
local t = "asddddddddddddddddddddddddddddddddddddddddddddddddd";

-- if args and args.x > 0 then 
--   global = if global then global + 1 else 0;
--   print("Global:", global);
-- end 

-- Luau
-- p.x += if args then args.x else 0;
-- p.t = if args and args.t then args.t else { t };
-- 
-- if p.t then 
--   p.t[#p.t + 1] = x .. "WOW";  
-- end
-- 
-- if p.x > 180 then
--   p.t = {}
-- end 

return p;