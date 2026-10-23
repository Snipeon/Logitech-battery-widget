-- Rainmeter calls this once when the skin loads. No one-time setup is needed.
function Initialize() return 0 end

-- Map each card number to the meter names defined in the INI files.
-- The smaller layouts define only the cards they can display.
local tiles = {
  {tile="Tile1", reflect="Reflect1", shine="Shine1", track="RingTrack1", ring="RingValue1", arc="RingArc1", percent="Percent1", name="Name1", charge="Charge1", measure="MeasureLevel1"},
  {tile="Tile2", reflect="Reflect2", shine="Shine2", track="RingTrack2", ring="RingValue2", arc="RingArc2", percent="Percent2", name="Name2", charge="Charge2", measure="MeasureLevel2"},
  {tile="Tile3", reflect="Reflect3", shine="Shine3", track="RingTrack3", ring="RingValue3", arc="RingArc3", percent="Percent3", name="Name3", charge="Charge3", measure="MeasureLevel3"},
  {tile="Tile4", reflect="Reflect4", shine="Shine4", track="RingTrack4", ring="RingValue4", arc="RingArc4", percent="Percent4", name="Name4", charge="Charge4", measure="MeasureLevel4"}
}
-- Top-left coordinates for each card and its contents in the 2x2 layout.
-- The same positions are reused by smaller layouts, which use only the first slots.
local slotPositions = {
  {tile={8,8}, ring={55,18}, percent={83,46}, name={83,74}, charge={83,88}},
  {tile={166,8}, ring={213,18}, percent={241,46}, name={241,74}, charge={241,88}},
  {tile={8,112}, ring={55,122}, percent={83,150}, name={83,178}, charge={83,192}},
  {tile={166,112}, ring={213,122}, percent={241,150}, name={241,178}, charge={241,192}}
}
-- Rust writes one tab-separated row per device to this shared snapshot file.
local dataFile = os.getenv("LOCALAPPDATA") .. "\\PeripheralBatteryWidget\\devices.tsv"
local currentLayout = nil

-- Change a meter setting and ask Rainmeter to redraw that meter.
local function setOption(meter, option, value)
  SKIN:Bang("!SetOption", meter, option, value)
  SKIN:Bang("!UpdateMeter", meter)
end
-- Change a measure setting and update it with Rainmeter's measure-specific command.
local function setMeasureOption(measure, option, value)
  SKIN:Bang("!SetOption", measure, option, value)
  SKIN:Bang("!UpdateMeasure", measure)
end
-- Show or hide every fixed part of a card together.
local function setVisible(tile, visible)
  local action = visible and "!ShowMeter" or "!HideMeter"
  for _, key in ipairs({"tile", "reflect", "shine", "track", "percent", "name", "charge"}) do SKIN:Bang(action, tile[key]) end
end
-- Build the circular battery arc Rainmeter draws for values below 100 percent.
local function arcShape(percent, color)
  local center, radius = 28, 24
  local startAngle = math.rad(270)
  local endAngle = startAngle + math.rad(360 * percent / 100)
  local sx, sy = center + radius * math.cos(startAngle), center + radius * math.sin(startAngle)
  local ex, ey = center + radius * math.cos(endAngle), center + radius * math.sin(endAngle)
  local arcSize = percent > 50 and 1 or 0
  return string.format("Arc %.2f,%.2f,%.2f,%.2f,%.2f,%.2f,0,0,%d,0 | StrokeWidth 6 | Stroke Color %s | StrokeStartCap Round | StrokeEndCap Round", sx, sy, ex, ey, radius, radius, arcSize, color)
end
-- Pick a ring color based on the current charge and battery level.
local function ringColor(percent, charging)
  if charging then return "61,174,239,225" end
  if percent <= 15 then return "239,86,86,225" end
  if percent <= 35 then return "241,177,67,225" end
  return "61,205,103,225"
end
local function fitDeviceName(name)
  -- Keep the model recognizable while preventing names from crossing card edges.
  local limit = 22
  if #name <= limit then return name end
  return name:sub(1, limit - 3) .. "..."
end
-- Read the helper's snapshot and turn each row into a Lua table for the cards.
local function addSnapshotRows(rows)
  local file = io.open(dataFile, "r")
  if not file then return end
  -- Each line contains a device name, percentage, charge state, and stale flag.
  for line in file:lines() do
    local name, percent, chargeState, stale = line:match("^(.-)\t(.-)\t([a-z]+)\t([01])$")
    if name then table.insert(rows, {name=name, percent=tonumber(percent), charging=chargeState == "charging" or chargeState == "full", chargeState=chargeState, stale=stale == "1"}) end
  end
  file:close()
end

function Update()
  -- Read the selected layout dimensions; use the full grid if the setting is invalid.
  local layout = (SKIN:GetVariable("Layout", "2x2") or "2x2"):lower()
  local cols, rowCount = layout:match("^(%d+)x(%d+)$")
  cols, rowCount = tonumber(cols), tonumber(rowCount)
  if not cols or not rowCount or cols < 1 or cols > 2 or rowCount < 1 or rowCount > 2 then layout, cols, rowCount = "2x2", 2, 2 end
  local visibleCount = cols * rowCount
  -- The 1x1 and 2x1 INIs contain only the card meters they need.
  local cardCount = layout == "1x1" and 1 or layout == "2x1" and 2 or 4
  if layout ~= currentLayout then
    -- Resize the background and position the meters when switching layouts.
    local width, height = cols * 158 + 8, rowCount * 104 + 8
    setOption("MeterBackground", "Shape", "Rectangle 0,0," .. width .. "," .. height .. ",12 | Fill LinearGradient PanelGradient | StrokeWidth 1 | Stroke LinearGradient PanelEdge")
    for i=1,cardCount do
      local m, p = tiles[i], slotPositions[i]
      local active = i <= visibleCount
      for _, key in ipairs({"tile", "ring", "percent", "name", "charge"}) do local point = active and p[key] or {0,0}; SKIN:Bang("!MoveMeter", point[1], point[2], m[key]) end
      SKIN:Bang("!MoveMeter", p.ring[1], p.ring[2], m.track)
      SKIN:Bang("!MoveMeter", p.ring[1], p.ring[2], m.arc)
    end
    currentLayout = layout
  end

  local rows = {}
  addSnapshotRows(rows)
  if #rows == 0 then
    -- Show a short hint instead of leaving an empty widget when no devices are ready.
    local message = "Waiting for supported devices..."
    setOption("MeterEmpty", "Text", message)
    SKIN:Bang("!ShowMeter", "MeterEmpty")
  else
    SKIN:Bang("!HideMeter", "MeterEmpty")
  end
  for i=1,cardCount do
    -- The helper sorts rows, so each visible row fills the next card in order.
    local meter, d = tiles[i], i <= visibleCount and rows[i] or nil
    if d then
      -- Put the device name and battery percent on this card.
      setOption(meter.name, "Text", fitDeviceName(d.name))
      local numeric = tonumber(d.percent)
      if numeric then
        -- Clamp unexpected values before using them in text or the ring drawing.
        numeric = math.max(0, math.min(100, numeric))
        setOption(meter.percent, "Text", math.floor(numeric + 0.5) .. "%")
        local chargeText = d.chargeState == "charging" and "CHARGING" or d.chargeState == "full" and "FULL" or d.chargeState == "fault" and "CHARGE FAULT" or ""
        setOption(meter.charge, "Text", d.stale and "RETRYING" or chargeText)
        -- Update the gauge and choose whether to show a full ring or a partial arc.
        setMeasureOption(meter.measure, "Formula", tostring(numeric))
        local color = ringColor(numeric, d.charging)
        setOption(meter.ring, "LineColor", color)
        if numeric >= 100 then SKIN:Bang("!ShowMeter", meter.ring); SKIN:Bang("!HideMeter", meter.arc)
        elseif numeric > 0 then setOption(meter.arc, "Shape", arcShape(numeric, color)); SKIN:Bang("!ShowMeter", meter.arc); SKIN:Bang("!HideMeter", meter.ring)
        else SKIN:Bang("!HideMeter", meter.ring); SKIN:Bang("!HideMeter", meter.arc) end
      else
        -- Keep showing the device if its name is known but its level is unavailable.
        local chargeText = d.chargeState == "charging" and "CHARGING" or d.chargeState == "full" and "FULL" or d.chargeState == "fault" and "CHARGE FAULT" or "BATTERY UNAVAILABLE"
        setOption(meter.percent, "Text", "--"); setOption(meter.charge, "Text", d.stale and "RETRYING" or chargeText)
        setMeasureOption(meter.measure, "Formula", "0")
        SKIN:Bang("!HideMeter", meter.ring); SKIN:Bang("!HideMeter", meter.arc)
      end
      setVisible(meter, true)
    else
      -- No device belongs in this slot, so hide all of its card elements.
      setVisible(meter, false); SKIN:Bang("!HideMeter", meter.ring); SKIN:Bang("!HideMeter", meter.arc)
    end
  end
  -- Redraw after all cards have been updated so they change together.
  SKIN:Bang("!Redraw")
  return 0
end
