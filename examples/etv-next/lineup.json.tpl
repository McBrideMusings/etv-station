{
  "server": {
    "bind_address": "${ETV_BIND_ADDRESS}",
    "port": ${ETV_PORT}
  },
  "output": {
    "folder": "tmp/hls"
  },
  "channels": [
    {
      "number": "1",
      "name": "etv-station test",
      "config": "./channel.json"
    },
    {
      "number": "2",
      "name": "Die Hard 24/7",
      "config": "./channel2.json"
    }
  ]
}
