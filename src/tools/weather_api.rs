use super::traits::{Tool, ToolResult};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};

const BASE_URL: &str = "https://api.weatherapi.com/v1";

/// WeatherAPI.com integration for current conditions and 7-day forecasts.
pub struct WeatherApiTool {
    client: Client,
}

impl WeatherApiTool {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
        }
    }
}

#[async_trait]
impl Tool for WeatherApiTool {
    fn name(&self) -> &str {
        "weather_api"
    }

    fn description(&self) -> &str {
        "Fetch current weather or a 7-day forecast using WeatherAPI.com"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "api_key": {
                    "type": "string",
                    "description": "WeatherAPI.com key (optional, defaults to WEATHER_API_KEY env var or onboarding config)"
                },
                "query": {
                    "type": "string",
                    "description": "City name, ZIP code, or lat,long to look up"
                },
                "days": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 7,
                    "description": "Number of days to forecast (1 = current conditions)"
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let arg_api_key = args
            .get("api_key")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let env_api_key = std::env::var("WEATHER_API_KEY")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        let api_key = arg_api_key
            .or(env_api_key)
            .ok_or_else(|| anyhow!(
                "WeatherAPI key not provided. Pass 'api_key', set WEATHER_API_KEY, or rerun `zeroclaw onboard`."
            ))?;
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("Missing 'query' parameter"))?;

        let raw_days = args.get("days").and_then(|v| v.as_i64()).unwrap_or(1);
        let normalized_days = raw_days.max(1).min(7) as u8;
        let endpoint = if normalized_days > 1 {
            "forecast.json"
        } else {
            "current.json"
        };

        let mut request = self
            .client
            .get(format!("{BASE_URL}/{endpoint}"))
            .query(&[("key", api_key.as_str()), ("q", query)]);

        if normalized_days > 1 {
            request = request.query(&[("days", normalized_days)]);
        }

        let response = match request.send().await {
            Ok(resp) => resp,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("WeatherAPI request failed: {e}")),
                })
            }
        };

        let status = response.status();
        let body = match response.text().await {
            Ok(text) => text,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to read WeatherAPI response: {e}")),
                })
            }
        };

        if !status.is_success() {
            let error_detail = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(|s| s.to_string()))
                .unwrap_or_else(|| body.clone());
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("WeatherAPI error ({status}): {error_detail}")),
            });
        }

        let parsed: Value = match serde_json::from_str(&body) {
            Ok(val) => val,
            Err(e) => return Err(anyhow!("Failed to parse WeatherAPI response: {e}")),
        };

        let pretty_body = serde_json::to_string_pretty(&parsed).unwrap_or(body);
        let summary = if normalized_days > 1 {
            summarize_forecast(&parsed, normalized_days.into())
                .unwrap_or_else(|| pretty_body.clone())
        } else {
            summarize_current(&parsed).unwrap_or_else(|| pretty_body.clone())
        };

        Ok(ToolResult {
            success: true,
            output: summary,
            error: None,
        })
    }
}

fn summarize_current(data: &Value) -> Option<String> {
    let location_line = build_location_line(data)?;
    let current = data.get("current")?;
    let condition = current.get("condition")?.get("text")?.as_str()?;
    let temp = current.get("temp_c")?.as_f64()?;
    let feels_like = current.get("feelslike_c")?.as_f64()?;
    let humidity = current.get("humidity")?.as_i64()?;
    let wind_kph = current.get("wind_kph")?.as_f64()?;
    let wind_dir = current
        .get("wind_dir")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let updated = current
        .get("last_updated")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    Some(format!(
        "{location_line}\nCurrent: {condition}, temp {temp:.1} C (feels {feels_like:.1} C), humidity {humidity}%\nWind: {wind_kph:.1} kph {wind_dir}\nLast updated: {updated}"
    ))
}

fn summarize_forecast(data: &Value, days: usize) -> Option<String> {
    let location_line = build_location_line(data)?;
    let forecast = data.get("forecast")?.get("forecastday")?.as_array()?;
    let requested = days.min(forecast.len());

    let mut lines = Vec::with_capacity(requested + 2);
    lines.push(location_line);
    lines.push(format!("Forecast (next {requested} day(s)):"));

    for day in forecast.iter().take(requested) {
        let date = day.get("date")?.as_str()?;
        let details = day.get("day")?;
        let condition = details.get("condition")?.get("text")?.as_str()?;
        let max = details.get("maxtemp_c")?.as_f64()?;
        let min = details.get("mintemp_c")?.as_f64()?;
        let rain_chance = extract_percentage(details.get("daily_chance_of_rain"));
        if let Some(rain) = rain_chance {
            lines.push(format!(
                "{date}: {condition}, min {min:.1} C / max {max:.1} C (rain chance {rain})"
            ));
        } else {
            lines.push(format!(
                "{date}: {condition}, min {min:.1} C / max {max:.1} C"
            ));
        }
    }

    Some(lines.join("\n"))
}

fn build_location_line(data: &Value) -> Option<String> {
    let location = data.get("location")?;
    let name = location.get("name")?.as_str()?;
    let country = location.get("country")?.as_str()?;
    let region = location
        .get("region")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let lat = location.get("lat").and_then(|v| v.as_f64());
    let lon = location.get("lon").and_then(|v| v.as_f64());

    let mut line = String::from("Location: ");
    line.push_str(name);
    line.push_str(", ");
    if let Some(r) = region {
        line.push_str(r);
        line.push_str(", ");
    }
    line.push_str(country);
    if let (Some(lat), Some(lon)) = (lat, lon) {
        line.push_str(&format!(" (lat {lat:.2}, lon {lon:.2})"));
    }

    Some(line)
}

fn extract_percentage(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else if trimmed.ends_with('%') {
            Some(trimmed.to_string())
        } else {
            Some(format!("{trimmed}%"))
        }
    } else if let Some(num) = value.as_f64() {
        Some(format!("{num:.0}%"))
    } else if let Some(num) = value.as_i64() {
        Some(format!("{num}%"))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_summary_formats() {
        let sample = json!({
            "location": {
                "name": "London",
                "region": "City of London",
                "country": "United Kingdom",
                "lat": 51.52,
                "lon": -0.11
            },
            "current": {
                "temp_c": 13.5,
                "feelslike_c": 12.1,
                "humidity": 82,
                "wind_kph": 10.2,
                "wind_dir": "SW",
                "last_updated": "2026-02-15 09:00",
                "condition": {"text": "Partly cloudy"}
            }
        });

        let summary = summarize_current(&sample).unwrap();
        assert!(summary.contains("London"));
        assert!(summary.contains("Partly cloudy"));
    }

    #[test]
    fn forecast_summary_formats_multiple_days() {
        let sample = json!({
            "location": {
                "name": "Newark",
                "region": "New Jersey",
                "country": "USA",
                "lat": 40.73,
                "lon": -74.17
            },
            "forecast": {
                "forecastday": [
                    {
                        "date": "2026-02-15",
                        "day": {
                            "maxtemp_c": 8.0,
                            "mintemp_c": -1.0,
                            "daily_chance_of_rain": 55,
                            "condition": {"text": "Light rain"}
                        }
                    },
                    {
                        "date": "2026-02-16",
                        "day": {
                            "maxtemp_c": 4.0,
                            "mintemp_c": -3.5,
                            "daily_chance_of_rain": 20,
                            "condition": {"text": "Sunny"}
                        }
                    }
                ]
            }
        });

        let summary = summarize_forecast(&sample, 3).unwrap();
        assert!(summary.contains("Forecast"));
        assert!(summary.contains("Light rain"));
        assert!(summary.contains("Sunny"));
        assert!(summary.contains("Forecast (next 2 day(s))"));
    }
}
