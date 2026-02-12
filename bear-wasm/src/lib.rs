use std::cell::RefCell;
use std::rc::Rc;

use bear_core::ClientMessage;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{CloseEvent, Event, MessageEvent, WebSocket};

#[wasm_bindgen]
pub struct BearWasmClient {
    websocket: Option<WebSocket>,
    outbound: Rc<RefCell<Vec<String>>>,
    on_message: js_sys::Function,
    on_error: js_sys::Function,
    _onmessage: Option<Closure<dyn FnMut(MessageEvent)>>,
    _onerror: Option<Closure<dyn FnMut(Event)>>,
    _onclose: Option<Closure<dyn FnMut(CloseEvent)>>,
}

#[wasm_bindgen]
impl BearWasmClient {
    #[wasm_bindgen(js_name = connectWebSocket)]
    pub fn connect_websocket(
        ws_url: String,
        on_message: js_sys::Function,
        on_error: js_sys::Function,
    ) -> Result<BearWasmClient, JsValue> {
        let websocket = WebSocket::new(&ws_url)?;
        let outbound = Rc::new(RefCell::new(Vec::new()));

        let on_message_closure = {
            let on_message = on_message.clone();
            Closure::wrap(Box::new(move |event: MessageEvent| {
                if let Some(text) = event.data().as_string() {
                    let _ = on_message.call1(&JsValue::NULL, &JsValue::from_str(&text));
                }
            }) as Box<dyn FnMut(_)> )
        };
        websocket.set_onmessage(Some(on_message_closure.as_ref().unchecked_ref()));

        let on_error_closure = {
            let on_error = on_error.clone();
            Closure::wrap(Box::new(move |_event: Event| {
                let _ = on_error.call1(&JsValue::NULL, &JsValue::from_str("websocket error"));
            }) as Box<dyn FnMut(_)> )
        };
        websocket.set_onerror(Some(on_error_closure.as_ref().unchecked_ref()));

        let on_close_closure = {
            let on_error = on_error.clone();
            Closure::wrap(Box::new(move |event: CloseEvent| {
                let message = format!("connection closed (code {})", event.code());
                let _ = on_error.call1(&JsValue::NULL, &JsValue::from_str(&message));
            }) as Box<dyn FnMut(_)> )
        };
        websocket.set_onclose(Some(on_close_closure.as_ref().unchecked_ref()));

        Ok(BearWasmClient {
            websocket: Some(websocket),
            outbound,
            on_message,
            on_error,
            _onmessage: Some(on_message_closure),
            _onerror: Some(on_error_closure),
            _onclose: Some(on_close_closure),
        })
    }

    #[wasm_bindgen(js_name = newProxy)]
    pub fn new_proxy(on_message: js_sys::Function, on_error: js_sys::Function) -> BearWasmClient {
        BearWasmClient {
            websocket: None,
            outbound: Rc::new(RefCell::new(Vec::new())),
            on_message,
            on_error,
            _onmessage: None,
            _onerror: None,
            _onclose: None,
        }
    }

    #[wasm_bindgen(js_name = sendInput)]
    pub fn send_input(&self, text: String) -> Result<(), JsValue> {
        let payload = serde_json::to_string(&ClientMessage::Input { text })
            .map_err(|err| JsValue::from_str(&err.to_string()))?;
        if let Some(ws) = &self.websocket {
            ws.send_with_str(&payload)?;
        } else {
            self.outbound.borrow_mut().push(payload);
        }
        Ok(())
    }

    #[wasm_bindgen(js_name = drainOutbound)]
    pub fn drain_outbound(&self) -> js_sys::Array {
        let mut queue = self.outbound.borrow_mut();
        let array = js_sys::Array::new();
        for message in queue.drain(..) {
            array.push(&JsValue::from_str(&message));
        }
        array
    }

    #[wasm_bindgen(js_name = feedServerMessage)]
    pub fn feed_server_message(&self, json: String) {
        let _ = self
            .on_message
            .call1(&JsValue::NULL, &JsValue::from_str(&json));
    }

    #[wasm_bindgen(js_name = close)]
    pub fn close(&self) -> Result<(), JsValue> {
        if let Some(ws) = &self.websocket {
            ws.close()?;
        }
        Ok(())
    }
}
