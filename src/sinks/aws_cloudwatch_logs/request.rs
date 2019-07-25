use super::CloudwatchError;
use futures::{sync::oneshot, try_ready, Future, Poll};
use rusoto_core::RusotoFuture;
use rusoto_logs::{
    CloudWatchLogs, CloudWatchLogsClient, CreateLogStreamError, CreateLogStreamRequest,
    DescribeLogStreamsError, DescribeLogStreamsRequest, DescribeLogStreamsResponse, InputLogEvent,
    PutLogEventsError, PutLogEventsRequest, PutLogEventsResponse,
};

pub struct CloudwatchFuture {
    client: Client,
    state: State,
    events: Option<Vec<InputLogEvent>>,
    token_tx: Option<oneshot::Sender<Option<String>>>,
}

struct Client {
    client: CloudWatchLogsClient,
    stream_name: String,
    group_name: String,
}

enum State {
    CreateStream(RusotoFuture<(), CreateLogStreamError>),
    DescribeStream(RusotoFuture<DescribeLogStreamsResponse, DescribeLogStreamsError>),
    Put(RusotoFuture<PutLogEventsResponse, PutLogEventsError>),
}

impl CloudwatchFuture {
    pub fn new(
        client: CloudWatchLogsClient,
        stream_name: String,
        group_name: String,
        events: Vec<InputLogEvent>,
        token: Option<String>,
        token_tx: oneshot::Sender<Option<String>>,
    ) -> Self {
        let client = Client {
            client,
            stream_name,
            group_name,
        };

        match token {
            Some(t) => {
                let fut = client.put_logs(Some(t), events);
                Self {
                    client,
                    events: None,
                    state: State::Put(fut),
                    token_tx: Some(token_tx),
                }
            }
            None => {
                trace!("Token does not exist; calling describe stream.");
                let fut = client.describe_stream();
                Self {
                    client,
                    events: Some(events),
                    state: State::DescribeStream(fut),
                    token_tx: Some(token_tx),
                }
            }
        }
    }

    fn transition_to_put(&mut self, token: Option<String>) {
        let events = self
            .events
            .take()
            .expect("Put got called twice, this is a bug!");

        trace!(message = "putting logs.", ?token);
        self.state = State::Put(self.client.put_logs(token, events));
    }
}

impl Client {
    fn put_logs(
        &self,
        sequence_token: Option<String>,
        log_events: Vec<InputLogEvent>,
    ) -> RusotoFuture<PutLogEventsResponse, PutLogEventsError> {
        let request = PutLogEventsRequest {
            log_events,
            sequence_token,
            log_group_name: self.group_name.clone(),
            log_stream_name: self.stream_name.clone(),
        };

        self.client.put_log_events(request)
    }

    fn describe_stream(&self) -> RusotoFuture<DescribeLogStreamsResponse, DescribeLogStreamsError> {
        let request = DescribeLogStreamsRequest {
            limit: Some(1),
            log_group_name: self.group_name.clone(),
            log_stream_name_prefix: Some(self.stream_name.clone()),
            ..Default::default()
        };

        self.client.describe_log_streams(request)
    }

    fn create_log_stream(&self) -> RusotoFuture<(), CreateLogStreamError> {
        let request = CreateLogStreamRequest {
            log_group_name: self.group_name.clone(),
            log_stream_name: self.stream_name.clone(),
        };

        self.client.create_log_stream(request)
    }
}

impl Future for CloudwatchFuture {
    type Item = ();
    type Error = CloudwatchError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            match &mut self.state {
                State::DescribeStream(fut) => {
                    let response = try_ready!(fut.poll().map_err(CloudwatchError::Describe));

                    if let Some(stream) = response
                        .log_streams
                        .ok_or(CloudwatchError::NoStreamsFound)?
                        .into_iter()
                        .next()
                    {
                        trace!(message = "stream found", stream = ?stream.log_stream_name);
                        self.transition_to_put(stream.upload_sequence_token);
                    } else {
                        trace!("provided stream does not exist; creating a new one.");
                        self.state = State::CreateStream(self.client.create_log_stream());
                    };
                }

                State::CreateStream(fut) => {
                    try_ready!(fut.poll().map_err(CloudwatchError::CreateStream));

                    trace!("stream created.");

                    // None is a valid token for a newly created stream
                    self.transition_to_put(None);
                }

                State::Put(fut) => {
                    let res = try_ready!(fut.poll().map_err(CloudwatchError::Put));
                    let next_token = res.next_sequence_token;

                    trace!(message = "putting logs was successful.", ?next_token);

                    self.token_tx
                        .take()
                        .expect("Put returned twice, this is a bug!")
                        .send(next_token)
                        .unwrap();

                    return Ok(().into());
                }
            }
        }
    }
}